//! 未保存変更がある状態で「タブを破棄する操作」 (終了 / タブを閉じる) をしようと
//! したときの保存確認ガードの回帰テスト。`docs/plan_project_tabs.md` §5.2: New / Open は
//! タブを置き換えない (新しいタブに開く) のでガードを通らない。
//!
//! 検証する状態機械 (`AppData`):
//! - `request_close` / `request_guarded_action`: dirty なら確認モーダルを開く /
//!   clean なら即操作実行
//! - `DirtyGuardDiscard`: 保存せず操作実行 (終了 / タブを閉じる)
//! - `DirtyGuardCancel`: 操作取りやめ (プロジェクト維持)
//! - `DirtyGuardSave` (同期): plugin 無し project は即保存 → 操作実行
//! - `DirtyGuardSave` (非同期): plugin 有り project は plugin state 取得
//!   (`AllStatesReceived`) を待ってから保存 → 操作実行

use common::protocol::{PluginCommand, PluginEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent, DirtyGuardAction};
use daw_gui::event_tabs::TabEvent;
use daw_gui::shutdown::QuitRequest;

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
    app.cur.song_doc.mark_saved();

    app.request_close();

    assert!(app.shutdown.is_shutting_down(), "clean project closes immediately");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "no confirm modal when clean");
}

#[test]
fn dirty_close_opens_confirm_modal() {
    let (mut app, _rx) = build_app();
    app.cur.song_doc.normalize(|_| {});

    app.request_close();

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "dirty project opens confirm modal for Quit"
    );
    assert!(!app.shutdown.is_shutting_down(), "must not quit before user decides");
}

#[test]
fn discard_quits_without_saving() {
    let (mut app, _rx) = build_app();
    app.cur.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardDiscard);

    assert!(app.shutdown.is_shutting_down(), "discard quits");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after discard");
}

#[test]
fn cancel_keeps_app_running() {
    let (mut app, _rx) = build_app();
    app.cur.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardCancel);

    assert!(!app.shutdown.is_shutting_down(), "cancel keeps running");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after cancel");
    assert!(app.cur.song_doc.is_dirty(), "cancel does not save");
}

#[test]
fn save_without_plugins_saves_synchronously_then_quits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    app.request_close();

    app.handle_event(AppEvent::DirtyGuardSave);

    assert!(path.exists(), "project file written: {}", path.display());
    assert!(!app.cur.song_doc.is_dirty(), "is_dirty cleared after save");
    assert!(app.shutdown.is_shutting_down(), "sync save quits immediately");
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
        app.cur.pipc.pending_state_queue.is_empty(),
        "queue empty after plugin load"
    );
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    app.request_close();

    // 「保存して終了」: plugin 有りなので save は非同期 (state 取得待ち)。
    app.handle_event(AppEvent::DirtyGuardSave);
    assert!(!app.shutdown.is_shutting_down(), "must wait for plugin states before quitting");
    assert_eq!(
        app.ui_ephemeral.guard_after_save,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "marked to quit after async save"
    );
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
    assert!(!path.exists(), "not saved yet (awaiting states)");

    // plugin state 到着 → 保存実行 → 終了確定。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));

    assert!(path.exists(), "project saved after states arrive");
    assert!(!app.cur.song_doc.is_dirty(), "is_dirty cleared after async save");
    assert!(app.shutdown.is_shutting_down(), "quits after async save completes");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "async-quit intent cleared");
}

// ---------------------------------------------------------------------------
// `docs/plan_project_tabs.md` §5.2: New / Open はタブを置き換えないのでガードを通らない。
// 破壊操作はタブを閉じる (`TabEvent::Close`) で、終了と同じガードを通る。
// ---------------------------------------------------------------------------

#[test]
fn new_opens_a_new_tab_without_guard_even_when_dirty() {
    let (mut app, _rx) = build_app();
    let path = std::path::PathBuf::from("C:/some/proj.daw");
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let first = app.pk();

    app.handle_event(AppEvent::New);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "New never asks (Q4)");
    assert_eq!(app.tabs.len(), 2, "New adds a tab");
    assert_ne!(app.pk(), first, "the new tab is active");
    assert!(app.cur.song_doc.file_path.is_none(), "fresh Untitled");
    assert!(!app.cur.song_doc.is_dirty(), "fresh project is clean");
    let old = app.tab(first).expect("first tab still open");
    assert_eq!(old.song_doc.file_path, Some(path), "old tab kept its file");
    assert!(old.song_doc.is_dirty(), "old tab keeps its unsaved edits");
}

#[test]
fn open_recent_into_dirty_tab_opens_a_new_tab() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("other.daw");
    let (mut app, _rx) = build_app();
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    app.cur.song_doc.normalize(|_| {});
    let first = app.pk();

    app.handle_event(AppEvent::OpenRecent(proj.clone()));

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "Open never asks (Q4)");
    assert_eq!(app.tabs.len(), 2, "opened into a new tab");
    assert_ne!(app.pk(), first);
    assert_eq!(app.cur.song_doc.file_path.as_ref(), Some(&proj));
    assert!(app.tab(first).unwrap().song_doc.is_dirty(), "old tab untouched");
}

#[test]
fn open_recent_into_pristine_tab_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("other.daw");
    let (mut app, _rx) = build_app();
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    let first = app.pk();

    app.handle_event(AppEvent::OpenRecent(proj.clone()));

    assert_eq!(app.tabs.len(), 1, "pristine Untitled is replaced, not kept");
    assert_eq!(app.pk(), first);
    assert_eq!(app.cur.song_doc.file_path.as_ref(), Some(&proj));
}

#[test]
fn open_recent_of_an_already_open_file_switches_to_its_tab() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("other.daw");
    let (mut app, _rx) = build_app();
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    let opened = app.pk();
    app.handle_event(AppEvent::New);
    assert_ne!(app.pk(), opened);

    app.handle_event(AppEvent::OpenRecent(proj.clone()));

    assert_eq!(app.pk(), opened, "switched back instead of opening twice");
    assert_eq!(app.tabs.len(), 2, "no third tab");
}

#[test]
fn dirty_close_tab_opens_guard_modal() {
    let (mut app, _rx) = build_app();
    let path = std::path::PathBuf::from("C:/some/proj.daw");
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();

    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "dirty close opens the confirm modal"
    );
    assert_eq!(app.cur.song_doc.file_path, Some(path), "tab not discarded yet");
    assert!(app.cur.song_doc.is_dirty(), "still dirty until resolved");
}

#[test]
fn dirty_close_tab_discard_closes_it_and_leaves_untitled() {
    let (mut app, _rx) = build_app();
    app.cur.song_doc.file_path = Some(std::path::PathBuf::from("C:/some/proj.daw"));
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));

    app.handle_event(AppEvent::DirtyGuardDiscard);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after discard");
    assert_eq!(app.tabs.len(), 1, "last tab closed → one Untitled remains (Q5)");
    assert_ne!(app.pk(), key, "the closed tab is gone");
    assert!(app.cur.song_doc.file_path.is_none(), "fresh Untitled");
    assert!(!app.cur.song_doc.is_dirty(), "fresh project is clean");
}

#[test]
fn dirty_close_tab_cancel_keeps_tab() {
    let (mut app, _rx) = build_app();
    let path = std::path::PathBuf::from("C:/some/proj.daw");
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));

    app.handle_event(AppEvent::DirtyGuardCancel);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed after cancel");
    assert_eq!(app.pk(), key, "tab kept");
    assert_eq!(app.cur.song_doc.file_path, Some(path));
    assert!(app.cur.song_doc.is_dirty(), "cancel does not discard or save");
}

#[test]
fn dirty_close_tab_save_then_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));

    // 「保存して閉じる」: plugin 無しなので同期保存 → 閉じる。
    app.handle_event(AppEvent::DirtyGuardSave);

    assert!(path.exists(), "project saved before closing");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "no async wait needed");
    assert_ne!(app.pk(), key, "tab closed after save");
    assert!(app.cur.song_doc.file_path.is_none(), "Untitled remains");
}

#[test]
fn close_others_asks_per_dirty_tab_in_order() {
    let (mut app, _rx) = build_app();
    let a = app.pk();
    app.cur.song_doc.normalize(|_| {}); // a: dirty
    app.handle_event(AppEvent::New);
    let b = app.pk();
    app.cur.song_doc.normalize(|_| {}); // b: dirty
    app.handle_event(AppEvent::New);
    let c = app.pk(); // c: clean, active

    app.handle_event(AppEvent::Tab(TabEvent::CloseOthers(c)));

    assert_eq!(app.pk(), a, "switched to the first dirty tab to ask");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![a, b])));
    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert_eq!(app.pk(), b, "then the second dirty tab");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![b])));
    app.handle_event(AppEvent::DirtyGuardCancel);
    assert_eq!(app.tabs.order, vec![b, c], "cancel stops the sequence; b survives");
}

#[test]
fn second_guarded_action_ignored_while_modal_open() {
    let (mut app, _rx) = build_app();
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![key])));

    // モーダル表示中に別のガード操作が来ても、 最初の判断待ちを維持する。
    app.handle_event(AppEvent::Tab(TabEvent::CloseAll));
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "modal stays on the first action while awaiting user decision"
    );
}

// ---------------------------------------------------------------------------
// レビュー指摘の回帰テスト (非同期保存との競合 / クラッシュ leak)。
// ---------------------------------------------------------------------------

/// blocker: 手動保存 (plugin state 待ちで非同期) が in-flight の最中にタブを閉じると、
/// 旧コードはモーダルを出し、 「保存して続行」 で 2 個目の Save を積み、
/// 保存完了 → 空 song に差し替わった後に dangling な 2 個目 Save が
/// **旧 path へ空プロジェクトを上書き**してデータを破壊した。 修正後は閉じるのを
/// 保存完了まで保留し、 実プロジェクトを保存してから閉じる。
#[test]
fn close_tab_during_in_flight_save_preserves_old_project() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // plugin あり → 保存は非同期 (state 待ち)。
    // 識別用に追加トラックを入れ、 「空 New ではない」 ことを後で検証する。
    let extra = app.cur.song_doc.song().tracks[0].clone();
    app.edit_song(|song| song.tracks.push(extra));
    let real_track_count = app.cur.song_doc.song().tracks.len();
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();

    // 手動 Ctrl+S → plugin state 待ちの非同期保存が in-flight。
    app.handle_event(AppEvent::Save);
    assert!(
        !app.cur.pipc.pending_state_queue.is_empty(),
        "manual save in flight (state request queued)"
    );

    // 保存中に閉じる。 モーダルは出さず、 queue drain まで保留する。
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "no modal opened mid-save");
    assert_eq!(
        app.cur.pipc.guard_pending_action,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "close deferred until the state queue drains"
    );

    // plugin state 到着 → 実プロジェクトを保存 → queue drain → 再評価で閉じる。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));

    let saved = common::project::load(&path).expect("project saved to disk");
    assert_eq!(
        saved.tracks.len(),
        real_track_count,
        "real project saved intact, NOT overwritten by an empty New"
    );
    assert_ne!(app.pk(), key, "tab closed after the save completed");
    assert!(app.cur.song_doc.file_path.is_none(), "Untitled remains");
    assert!(app.cur.pipc.guard_pending_action.is_none(), "deferred action consumed");
    assert!(app.ui_ephemeral.guard_after_save.is_none(), "no save-after intent left");
    assert!(
        app.cur.pipc.pending_state_queue.is_empty(),
        "no dangling save left in the queue"
    );
}

/// blocker (2nd review): 閉じるは **Save 以外**の plugin-state round-trip
/// (Deferred edit = DeleteTrack/Cut/Ungroup/RemoveDevice、 Copy) が in-flight の間も
/// 走らせてはいけない。 走らせると round-trip 完了処理 (track_id ベースの削除等) が
/// 閉じた後の別 project に誤適用される。 初版は `is_async_save_pending` (Save のみ)
/// で gate していて漏れていた。 修正後は pending_state_queue 全体で gate し、 drain 後に
/// 最新 dirty 状態で再評価する。
#[test]
fn close_tab_during_deferred_edit_defers_then_reevaluates() {
    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // plugin あり → DeleteTracks は deferred round-trip。
    // 削除対象に 2 本目のトラックを用意 (id は必ず採番し直す — clone のまま push すると
    // 同 id が 2 本並び、 安定 id での削除が意図しない方を指す)。
    let extra = app.cur.song_doc.song().tracks[0].clone();
    let target_id = app
        .edit_song(|song| {
            let id = song.alloc_track_id();
            let mut t = extra;
            t.id = id;
            song.tracks.push(t);
            id
        })
        .expect("edit_song");
    let key = app.pk();

    // DeleteTracks → Deferred(DeleteTracks) を enqueue (= state round-trip in flight)。
    // 削除自体は完了時に実行されるので、 この時点では song 未変更。
    app.handle_event(AppEvent::DeleteTracks(vec![target_id]));
    assert!(
        !app.cur.pipc.pending_state_queue.is_empty(),
        "deferred delete round-trip in flight"
    );

    // round-trip 中に閉じる。 Song を捨てず、 モーダルも出さず保留する。
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert!(
        app.ui_ephemeral.dirty_guard.is_none(),
        "no modal while a deferred round-trip is in flight"
    );
    assert_eq!(
        app.cur.pipc.guard_pending_action,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "close deferred until the whole state queue drains (not just Save)"
    );

    // round-trip 完了 → 削除実行で project が dirty 化 → drain 後に閉じるを再評価
    // → dirty なので確認モーダルを開く (= 黙って捨てない)。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    assert!(app.cur.pipc.guard_pending_action.is_none(), "deferred guard consumed");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "re-evaluated to a save-confirm dialog after the delete applied"
    );
    assert_eq!(app.pk(), key, "tab still open");
}

/// ガードモーダル表示中の手動保存は無視する (= 余分な Save を queue に積まない)。
/// plugin 無しなら action_save は同期実行されファイルが書かれてしまうので、
/// gate が効いていればファイルは作られない。
#[test]
fn manual_save_ignored_while_guard_modal_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app(); // plugin 無し → 保存は同期。
    app.cur.song_doc.file_path = Some(path.clone());
    app.cur.song_doc.normalize(|_| {});
    let key = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Close(key))); // dirty → モーダル。
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![key])));

    app.handle_event(AppEvent::Save);

    assert!(!path.exists(), "manual Save gated while the guard modal is open");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "modal still awaiting the user's decision"
    );
}

/// major: plugin host がクラッシュすると in-flight 保存は完了しない。 旧コードは
/// `guard_after_save` を Some のまま残し、 以後 閉じる/終了(✕) が
/// `request_guarded_action` の早期 return で恒久ロックされた。 修正後は disconnect
/// で stuck state を破棄し、 ガードが再び機能する。
///
/// (r.md #61) **終了意図だけは捨てずに聞き直す**。破棄系 (タブを閉じる) は
/// 保存が成立していない状態で捨てると未保存変更を失うので実行
/// しないが、終了は song を触らないので「保存して終了しますか」を最新状態で
/// 問い直すのが正しい (黙って消すと ✕ が効かなかったようにしか見えない)。
#[test]
fn plugin_host_disconnect_reasks_quit_and_unblocks_dirty_guard() {
    let (mut app, _rx) = build_app();
    let key = app.pk();
    // 非同期 round-trip 待ちで両方の deferred ガード state が立った状況を模す。
    app.ui_ephemeral.guard_after_save = Some(DirtyGuardAction::Quit(QuitRequest::USER));
    app.cur.pipc.guard_pending_action = Some(DirtyGuardAction::CloseTabs(vec![key]));
    app.cur.song_doc.normalize(|_| {});

    app.handle_event(AppEvent::Plugin(PluginEvent::ChildDisconnected));

    assert!(
        app.ui_ephemeral.guard_after_save.is_none(),
        "disconnect drops the stuck save-after action"
    );
    assert!(
        app.cur.pipc.guard_pending_action.is_none(),
        "disconnect drops the stuck queue-drain action"
    );
    assert!(
        app.cur.pipc.pending_state_queue.is_empty(),
        "stale state-request queue drained"
    );
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "quit intent survives the disconnect and is re-asked"
    );
    assert!(!app.shutdown.is_shutting_down(), "does not quit silently");

    // 聞き直しに答えれば、 以後ふたたびガードが開ける (= ロックされていない)。
    app.handle_event(AppEvent::DirtyGuardCancel);
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::CloseTabs(vec![key])),
        "dirty guard works again after a plugin host disconnect"
    );
}

/// 実機検証で発覚: ダーティーな project を「保存せず閉じる」 で破棄して同じ file を
/// 開き直すと、 破棄したはずの未保存変更を写した autosave sidecar が残っていて
/// recovery modal (「オートセーブデータがあります」) が出てしまっていた。
/// discard 時にそのタブの autosave を消すことで、 矛盾した復元提示を防ぐ。
#[test]
fn discard_then_reopen_same_file_has_no_recovery_modal() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    // 実在する .daw を作る (Open でロードできるように)。
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    app.cur.song_doc.file_path = Some(proj.clone());
    app.cur.song_doc.normalize(|_| {});
    // 未保存変更を写した sidecar autosave を用意 (.daw より後に書くので newer)。
    let sidecar = common::recovery::sidecar_for(&proj);
    common::project::save(&sidecar, app.cur.song_doc.song()).expect("write sidecar autosave");
    assert!(sidecar.exists(), "sidecar autosave staged");
    let key = app.pk();

    // タブを閉じる → ダーティーガード → 「保存せず閉じる」 → 同じ file を Open。
    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![key])));
    app.handle_event(AppEvent::DirtyGuardDiscard);
    app.handle_event(AppEvent::OpenRecent(proj.clone()));

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
        app.cur.song_doc.file_path.as_ref(),
        Some(&proj),
        "the same project was reopened"
    );
    assert_eq!(app.tabs.len(), 1, "reopened into the pristine Untitled left by the close");
}
