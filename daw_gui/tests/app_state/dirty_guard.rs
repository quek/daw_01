//! 未保存変更がある状態で「現在のプロジェクトを破棄する操作」 (終了 / New /
//! Open / Open Recent) をしようとしたときの保存確認ガードの回帰テスト。
//!
//! 検証する状態機械 (`AppData`):
//! - `request_close` / `request_guarded_action`: dirty なら確認モーダルを開く /
//!   clean なら即操作実行
//! - `DirtyGuardDiscard`: 保存せず操作実行 (終了 / New)
//! - `DirtyGuardCancel`: 操作取りやめ (プロジェクト維持)
//! - `DirtyGuardSave` (同期): plugin 無し project は即保存 → 操作実行
//! - `DirtyGuardSave` (非同期): plugin 有り project は plugin state 取得
//!   (`AllStatesReceived`) を待ってから保存 → 操作実行

use common::protocol::{PluginCommand, PluginEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent, DirtyGuardAction};

use super::support::{self, load_instrument};

/// 旧 dirty_guard.rs 独立バイナリ時代のシグネチャを保つ thin adapter。
/// audio_rx をここで drop する (= closed channel で走る) のも旧挙動の保存。
fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (app, _audio_rx, plugin_rx, _dispatcher) = support::build_app();
    (app, plugin_rx)
}

// ---------------------------------------------------------------------------
// 終了 (Quit) ケース — 旧 close_confirm テストを DirtyGuard へ移行。
// ---------------------------------------------------------------------------

#[test]
fn not_dirty_close_quits_immediately() {
    let (mut app, _rx) = build_app();
    app.song_doc.mark_saved();

    app.request_close();

    assert!(app.ui_ephemeral.should_quit, "clean project closes immediately");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "no confirm modal when clean");
}

#[test]
fn dirty_close_opens_confirm_modal() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});

    app.request_close();

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit),
        "dirty project opens confirm modal for Quit"
    );
    assert!(!app.ui_ephemeral.should_quit, "must not quit before user decides");
}

#[test]
fn discard_quits_without_saving() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardDiscard);

    assert!(app.ui_ephemeral.should_quit, "discard quits");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after discard");
    assert!(app.song_doc.is_dirty(), "discard does not save (still dirty)");
}

#[test]
fn cancel_keeps_app_running() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardCancel);

    assert!(!app.ui_ephemeral.should_quit, "cancel keeps running");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after cancel");
    assert!(app.song_doc.is_dirty(), "cancel does not save");
}

#[test]
fn save_without_plugins_saves_synchronously_then_quits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardSave);

    assert!(path.exists(), "project file written: {}", path.display());
    assert!(!app.song_doc.is_dirty(), "is_dirty cleared after save");
    assert!(app.ui_ephemeral.should_quit, "sync save quits immediately");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "no async wait needed");
}

#[test]
fn save_with_plugins_waits_for_states_then_quits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app);
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "queue empty after plugin load"
    );
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.request_close();

    // 「保存して終了」: plugin 有りなので save は非同期 (state 取得待ち)。
    app.handle_event(AppEvent::DirtyGuardSave);
    assert!(!app.ui_ephemeral.should_quit, "must wait for plugin states before quitting");
    assert_eq!(
        app.ui_ephemeral.guard_after_save,
        Some(DirtyGuardAction::Quit),
        "marked to quit after async save"
    );
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
    assert!(!path.exists(), "not saved yet (awaiting states)");

    // plugin state 到着 → 保存実行 → 終了確定。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));

    assert!(path.exists(), "project saved after states arrive");
    assert!(!app.song_doc.is_dirty(), "is_dirty cleared after async save");
    assert!(app.ui_ephemeral.should_quit, "quits after async save completes");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "async-quit intent cleared");
}

// ---------------------------------------------------------------------------
// New / Open / Open Recent も同じガードを通る。
// ---------------------------------------------------------------------------

#[test]
fn clean_new_runs_immediately() {
    let (mut app, _rx) = build_app();
    app.song_doc.file_path = Some(std::path::PathBuf::from("C:/some/proj.daw"));
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::New);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "clean New does not open modal");
    assert!(
        app.song_doc.file_path.is_none(),
        "New cleared file_path (action_new ran)"
    );
    assert!(!app.song_doc.is_dirty(), "fresh project is clean");
}

#[test]
fn dirty_new_opens_guard_modal() {
    let (mut app, _rx) = build_app();
    let path = std::path::PathBuf::from("C:/some/proj.daw");
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});

    app.handle_event(AppEvent::New);

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::New),
        "dirty New opens confirm modal for New"
    );
    // action_new はまだ走っていない (= 現プロジェクトは破棄されていない)。
    assert_eq!(app.song_doc.file_path, Some(path), "project not discarded yet");
    assert!(app.song_doc.is_dirty(), "still dirty until resolved");
}

#[test]
fn dirty_new_discard_runs_new() {
    let (mut app, _rx) = build_app();
    app.song_doc.file_path = Some(std::path::PathBuf::from("C:/some/proj.daw"));
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);

    app.handle_event(AppEvent::DirtyGuardDiscard);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after discard");
    assert!(app.song_doc.file_path.is_none(), "New ran (file_path cleared)");
    assert!(!app.song_doc.is_dirty(), "fresh project is clean");
}

#[test]
fn dirty_new_cancel_keeps_project() {
    let (mut app, _rx) = build_app();
    let path = std::path::PathBuf::from("C:/some/proj.daw");
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);

    app.handle_event(AppEvent::DirtyGuardCancel);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after cancel");
    assert_eq!(app.song_doc.file_path, Some(path), "project kept (New cancelled)");
    assert!(app.song_doc.is_dirty(), "cancel does not discard or save");
}

#[test]
fn dirty_new_save_then_runs_new() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);

    // 「保存して続行」: plugin 無しなので同期保存 → New 実行。
    app.handle_event(AppEvent::DirtyGuardSave);

    assert!(path.exists(), "current project saved before New");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "no async wait needed");
    assert!(app.song_doc.file_path.is_none(), "New ran after save (file_path cleared)");
    assert!(!app.song_doc.is_dirty(), "fresh project is clean");
}

#[test]
fn dirty_open_recent_opens_guard_with_path() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});
    let target = std::path::PathBuf::from("C:/some/other.daw");

    app.handle_event(AppEvent::OpenRecent(target.clone()));

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::OpenPath(target)),
        "dirty Open Recent opens confirm modal carrying the target path"
    );
}

#[test]
fn dirty_open_opens_guard_modal() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});

    app.handle_event(AppEvent::Open);

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Open),
        "dirty Open opens confirm modal for Open"
    );
}

#[test]
fn second_guarded_action_ignored_while_modal_open() {
    let (mut app, _rx) = build_app();
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::New));

    // モーダル表示中に別のガード操作が来ても、 最初の判断待ちを維持する。
    app.handle_event(AppEvent::Open);
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::New),
        "modal stays on the first action while awaiting user decision"
    );
}

// ---------------------------------------------------------------------------
// レビュー指摘の回帰テスト (非同期保存との競合 / クラッシュ leak)。
// ---------------------------------------------------------------------------

/// blocker: 手動保存 (plugin state 待ちで非同期) が in-flight の最中に New すると、
/// 旧コードはモーダルを出し、 「保存して続行」 で 2 個目の Save を積み、
/// 保存完了 → action_new で空 song に差し替わった後に dangling な 2 個目 Save が
/// **旧 path へ空プロジェクトを上書き**してデータを破壊した。 修正後は New を
/// 保存完了まで保留し、 実プロジェクトを保存してから New を実行する。
#[test]
fn new_during_in_flight_save_preserves_old_project() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // plugin あり → 保存は非同期 (state 待ち)。
    // 識別用に追加トラックを入れ、 「空 New ではない」 ことを後で検証する。
    let extra = app.song_doc.song().tracks[0].clone();
    app.edit_song(|song| song.tracks.push(extra));
    let real_track_count = app.song_doc.song().tracks.len();
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});

    // 手動 Ctrl+S → plugin state 待ちの非同期保存が in-flight。
    app.handle_event(AppEvent::Save);
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "manual save in flight (state request queued)"
    );

    // 保存中に New。 モーダルは出さず、 queue drain まで保留する。
    app.handle_event(AppEvent::New);
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "no modal opened mid-save");
    assert_eq!(
        app.ui_ephemeral.guard_pending_action,
        Some(DirtyGuardAction::New),
        "New deferred until the state queue drains"
    );

    // plugin state 到着 → 実プロジェクトを保存 → queue drain → 再評価で New を実行。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));

    let saved = common::project::load(&path).expect("project saved to disk");
    assert_eq!(
        saved.tracks.len(),
        real_track_count,
        "real project saved intact, NOT overwritten by an empty New"
    );
    assert!(app.song_doc.file_path.is_none(), "New ran after the save completed");
    assert!(app.ui_ephemeral.guard_pending_action.is_none(), "deferred action consumed");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "no save-after intent left");
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "no dangling save left in the queue"
    );
}

/// blocker (2nd review): New/Open は **Save 以外**の plugin-state round-trip
/// (Deferred edit = DeleteTrack/Cut/Ungroup/RemoveDevice、 Copy) が in-flight の間も
/// 走らせてはいけない。 走らせると round-trip 完了処理 (track_id ベースの削除等) が
/// 差し替え後の別 project に誤適用される。 初版は `is_async_save_pending` (Save のみ)
/// で gate していて漏れていた。 修正後は pending_state_queue 全体で gate し、 drain 後に
/// 最新 dirty 状態で再評価する。
#[test]
fn open_recent_during_deferred_edit_defers_then_reevaluates() {
    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // plugin あり → DeleteTrack は deferred round-trip。
    // 削除対象に 2 本目のトラックを用意。
    let extra = app.song_doc.song().tracks[0].clone();
    app.edit_song(|song| song.tracks.push(extra));
    let target_idx = (app.song_doc.song().tracks.len() - 1) as u32;

    // DeleteTrack → Deferred(DeleteTrack) を enqueue (= state round-trip in flight)。
    // 削除自体は完了時に実行されるので、 この時点では song 未変更。
    app.handle_event(AppEvent::DeleteTrack(target_idx));
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "deferred delete round-trip in flight"
    );

    // round-trip 中に Open Recent。 self.song_doc.song() を差し替えず、 モーダルも出さず保留する。
    let target = std::path::PathBuf::from("C:/some/other.daw");
    app.handle_event(AppEvent::OpenRecent(target.clone()));
    assert!(
        app.ui_ephemeral.dirty_guard.is_none(),
        "no modal while a deferred round-trip is in flight"
    );
    assert_eq!(
        app.ui_ephemeral.guard_pending_action,
        Some(DirtyGuardAction::OpenPath(target.clone())),
        "Open deferred until the whole state queue drains (not just Save)"
    );

    // round-trip 完了 → 削除実行で project が dirty 化 → drain 後に Open を再評価
    // → dirty なので確認モーダルを開く (= 黙って差し替えない)。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));
    assert!(app.ui_ephemeral.guard_pending_action.is_none(), "deferred guard consumed");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::OpenPath(target)),
        "re-evaluated to a save-confirm dialog after the delete applied"
    );
}

/// ガードモーダル表示中の手動保存は無視する (= 余分な Save を queue に積まない)。
/// plugin 無しなら action_save は同期実行されファイルが書かれてしまうので、
/// gate が効いていればファイルは作られない。
#[test]
fn manual_save_ignored_while_guard_modal_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app(); // plugin 無し → 保存は同期。
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New); // dirty → モーダル (保存 in-flight 無し)。
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::New));

    app.handle_event(AppEvent::Save);

    assert!(!path.exists(), "manual Save gated while the guard modal is open");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::New),
        "modal still awaiting the user's decision"
    );
}

/// major: plugin host がクラッシュすると in-flight 保存は完了しない。 旧コードは
/// `guard_after_save` を Some のまま残し、 以後 New/Open/終了(✕) が
/// `request_guarded_action` の早期 return で恒久ロックされた。 修正後は disconnect
/// で stuck state を破棄し、 ガードが再び機能する。
#[test]
fn plugin_host_disconnect_unblocks_dirty_guard() {
    

    let (mut app, _rx) = build_app();
    // 非同期 round-trip 待ちで両方の deferred ガード state が立った状況を模す。
    app.ui_ephemeral.guard_after_save = Some(DirtyGuardAction::Quit);
    app.ui_ephemeral.guard_pending_action = Some(DirtyGuardAction::New);

    app.handle_event(AppEvent::Plugin(PluginEvent::ChildDisconnected));

    assert!(
        app.ui_ephemeral.guard_after_save.is_none(),
        "disconnect drops the stuck save-after action"
    );
    assert!(
        app.ui_ephemeral.guard_pending_action.is_none(),
        "disconnect drops the stuck queue-drain action"
    );
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "stale state-request queue drained"
    );

    // 以後ふたたびガードが開ける (= ロックされていない)。
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::New),
        "dirty guard works again after a plugin host disconnect"
    );
}

/// 実機検証で発覚: ダーティーな project を「保存せず続行」 で破棄して同じ file を
/// 開き直すと、 破棄したはずの未保存変更を写した autosave sidecar が残っていて
/// recovery modal (「オートセーブデータがあります」) が出てしまっていた。
/// discard 時に現プロジェクトの autosave を消すことで、 矛盾した復元提示を防ぐ。
#[test]
fn discard_then_reopen_same_file_has_no_recovery_modal() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    // 実在する .daw を作る (Open でロードできるように)。
    common::project::save(&proj, app.song_doc.song()).expect("write project file");
    app.song_doc.file_path = Some(proj.clone());
    app.song_doc.normalize(|_| {});
    // 未保存変更を写した sidecar autosave を用意 (.daw より後に書くので newer)。
    let sidecar = common::recovery::sidecar_for(&proj);
    common::project::save(&sidecar, app.song_doc.song()).expect("write sidecar autosave");
    assert!(sidecar.exists(), "sidecar autosave staged");

    // 同じ file を Open → ダーティーガード → 「保存せず続行」。
    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::OpenPath(proj.clone())),
        "open same file while dirty opens the guard"
    );
    app.handle_event(AppEvent::DirtyGuardDiscard);

    // 破棄した変更の sidecar は消え、 recovery modal も出ない。
    assert!(
        !sidecar.exists(),
        "discarded project's autosave sidecar is removed"
    );
    assert!(
        !app.ui_ephemeral.show_recovery_modal,
        "no recovery modal after discarding then reopening the same file"
    );
    assert!(
        app.ui_ephemeral.recovery_candidates.is_empty(),
        "no stale recovery candidate"
    );
    assert_eq!(
        app.song_doc.file_path.as_ref(),
        Some(&proj),
        "the same project was reopened"
    );
}
