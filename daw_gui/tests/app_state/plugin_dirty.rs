//! r.md #160: plugin の中だけが変わったプロジェクトを「未保存」として扱う (`handler::plugin_dirty`)。
//!
//! plugin の state は Song の外にあるので、plugin の窓だけを触った変更は Song の dirty に現れず、閉じても
//! 保存確認が出ずに失われていた。ここは 2 段を見る: host が報告した変更で `*` が立つこと (保存の往復の間に
//! 来た報告を降ろさないこと)、報告の無い変更を閉じる前の state の比較で拾うこと。

use std::path::Path;
use std::time::{Duration, Instant};

use common::protocol::{PluginCommand, PluginEvent, ProjectKey, SlotState};

use daw_gui::app::{AppData, AppEvent, DirtyGuardAction};
use daw_gui::event_tabs::TabEvent;
use daw_gui::shutdown::QuitRequest;

use super::support::{build_app, drain, load_instrument, select_track_single, send_plugin_loaded};

/// plugin host の `RequestAllStates` 応答を模す (アクティブなタブの `device_id` の今の state = `data`)。
fn respond(app: &mut AppData, device_id: u64, data: &[u8]) {
    respond_to(app, app.pk(), device_id, data);
}

/// [`respond`] の、宛先のタブを指定する版 (背景タブ宛の応答)。そのタブがいま待っている要求への応答。
fn respond_to(app: &mut AppData, project: ProjectKey, device_id: u64, data: &[u8]) {
    let request = app.with_project(project, |app| app.awaited_state_request()).expect("tab is open");
    respond_as(app, project, request, device_id, data);
}

/// 要求の番号を指定して応答する (待っていない番号 = 破棄した要求の遅れた応答)。
fn respond_as(app: &mut AppData, project: ProjectKey, request: u64, device_id: u64, data: &[u8]) {
    let entries = vec![SlotState { device_id, data: Some(data.to_vec()), ara_archive: None, error: None }];
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project, request, entries }));
}

/// いま host へ出ている `RequestAllStates` の番号 (`msgs` の最後の要求)。
fn last_request(msgs: &[PluginCommand]) -> u64 {
    msgs.iter()
        .rev()
        .find_map(|m| match m {
            PluginCommand::RequestAllStates { request, .. } => Some(*request),
            _ => None,
        })
        .expect("a RequestAllStates was sent")
}

/// 楽器を 1 つ載せ、読み込みが確定した直後 (基準の state の要求が host へ出たところ) の app と device id。
fn plugin_just_loaded() -> (AppData, tokio::sync::mpsc::UnboundedReceiver<PluginCommand>, u64) {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let track_id = app.cur.song_doc.song().tracks[0].id;
    select_track_single(&mut app, 0);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb { id: "test.synth".into(), keep_open: false, open_gui: true });
    drain(&mut plugin_rx);
    let device_id = send_plugin_loaded(&mut app, track_id, 0, "test.synth", None);
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "読み込みが確定したら基準の state を取りに行く");
    (app, plugin_rx, device_id)
}

/// 保存してあったプロジェクトを開いた直後: 読み込みの後で host が返した state = `state`、未保存の印なし。
fn opened_project_with_plugin(
    state: &[u8],
) -> (AppData, tokio::sync::mpsc::UnboundedReceiver<PluginCommand>, u64) {
    let (mut app, plugin_rx, device_id) = plugin_just_loaded();
    respond(&mut app, device_id, state);
    app.cur.song_doc.mark_saved();
    (app, plugin_rx, device_id)
}

fn requests(msgs: &[PluginCommand]) -> usize {
    msgs.iter().filter(|m| matches!(m, PluginCommand::RequestAllStates { .. })).count()
}

/// host の報告: ユーザーが plugin の窓の中で中身を変えた。
fn plugin_edited(app: &mut AppData, device_id: u64) {
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginStateChanged { device: app.dev(device_id) }));
}

/// 楽器を 1 つ載せ、`path` へ保存した直後 (未保存の印なし、plugin の state = `state`) の app と device id。
/// 保存が plugin から取った state が、閉じるときに比べる基準になる。
fn saved_project_with_plugin(
    path: &Path,
    state: &[u8],
) -> (AppData, tokio::sync::mpsc::UnboundedReceiver<PluginCommand>, u64) {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    load_instrument(&mut app);
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    app.cur.song_doc.file_path = Some(path.to_path_buf());
    app.handle_event(AppEvent::Save);
    respond(&mut app, device_id, state);
    assert!(path.exists() && !app.cur.song_doc.is_dirty(), "saved: {}", app.ui_ephemeral.status_message);
    drain(&mut plugin_rx);
    (app, plugin_rx, device_id)
}

#[test]
fn plugin_の窓での変更は未保存になり_保存の往復の間に来た報告は降ろさない() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);

    plugin_edited(&mut app, device_id);
    assert!(app.cur.song_doc.is_dirty() && app.any_tab_dirty());
    assert!(app.window_title().starts_with('*'), "{}", app.window_title());

    // 保存の state を取りに行っている間に、もう一度触った。応答の state にそれが入っている保証は無い。
    app.handle_event(AppEvent::Save);
    plugin_edited(&mut app, device_id);
    respond(&mut app, device_id, &[2]);
    assert!(app.cur.song_doc.is_dirty(), "要求を送った後の変更は、この保存では降ろさない");

    app.handle_event(AppEvent::Save);
    respond(&mut app, device_id, &[3]);
    assert!(!app.cur.song_doc.is_dirty(), "変更の後に取り直した保存で降りる");

    // plugin のツマミは undo で戻らないので、Song を保存時点へ戻しても未保存のまま。
    plugin_edited(&mut app, device_id);
    let track = app.cur.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::SetTrackVolume { track, amp: 0.5 });
    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.is_dirty());
}

#[test]
fn 消した_device_宛ての遅れた報告は未保存にしない() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);
    plugin_edited(&mut app, device_id + 1000);
    assert!(!app.cur.song_doc.is_dirty());
}

#[test]
fn 触っていなければ_state_を比べて黙って終了する() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, mut plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "閉じる前に state を取り直す");
    assert!(!app.shutdown.is_shutting_down() && app.ui_ephemeral.dirty_guard.is_none(), "応答までは何も決めない");

    respond(&mut app, device_id, &[1]);
    assert!(app.shutdown.is_shutting_down(), "基準と同じ → 確認なしで終了");
    assert!(app.ui_ephemeral.dirty_guard.is_none());
    assert_eq!(requests(&drain(&mut plugin_rx)), 0, "同じ終了の中で 2 度は比べない");
}

#[test]
fn 報告の無い変更は閉じる前の比較で見つけて保存を聞く() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, mut plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    respond(&mut app, device_id, &[2]);
    assert!(app.cur.song_doc.is_dirty(), "基準と違う → 未保存");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));
    assert!(!app.shutdown.is_shutting_down());

    // キャンセルして、もう一度閉じようとしたら (もう未保存なので) 比べずに聞く。
    app.handle_event(AppEvent::DirtyGuardCancel);
    drain(&mut plugin_rx);
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert_eq!(requests(&drain(&mut plugin_rx)), 0);
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));

    // 「保存せず終了」は聞き直さずに終わる (捨てた後でもう一度比べると、同じ差でまた聞くことになる)。
    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert!(app.shutdown.is_shutting_down());
    assert_eq!(requests(&drain(&mut plugin_rx)), 0);
}

#[test]
fn 保存して終了は_取り直した_state_を書いて終わる() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");
    let (mut app, mut plugin_rx, device_id) = saved_project_with_plugin(&path, &[1]);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    respond(&mut app, device_id, &[2]);
    drain(&mut plugin_rx);
    app.handle_event(AppEvent::DirtyGuardSave);
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "保存の state の取り直し");
    respond(&mut app, device_id, &[2]);
    assert!(app.shutdown.is_shutting_down(), "{}", app.ui_ephemeral.status_message);
    assert_eq!(requests(&drain(&mut plugin_rx)), 0, "保存した state をもう一度比べない");

    let saved = common::project::load_project(&path).expect("reload");
    let state = saved.song.tracks[0].plugins().next().expect("synth").state.clone();
    assert_eq!(state.as_deref(), Some(&[2u8][..]), "保存したファイルに変更後の state が入っている");
}

/// 保存していなくても比べられる: 基準は、読み込みが確定した直後に plugin から取った state。
#[test]
fn 開いた直後に取った_state_を基準に比べる() {
    let (mut app, mut plugin_rx, device_id) = opened_project_with_plugin(&[1]);
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert_eq!(requests(&drain(&mut plugin_rx)), 1);
    respond(&mut app, device_id, &[1]);
    assert!(app.shutdown.is_shutting_down(), "開いて、触らずに閉じる → 聞かない");

    let (mut app, _plugin_rx, device_id) = opened_project_with_plugin(&[1]);
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    respond(&mut app, device_id, &[2]);
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));
}

/// 基準の応答が来ないまま往復を破棄したら、取り直す (取り直さないと、その plugin は次の保存まで比べられない)。
/// ユーザーが頼んだ操作ではないので「中止しました」とは出さない。
#[test]
fn 基準の応答が来なければ取り直す() {
    let (mut app, mut plugin_rx, device_id) = plugin_just_loaded();
    app.poll_state_roundtrip_watchdog(Instant::now() + Duration::from_secs(120));
    assert!(app.cur.pipc.pending_state_queue.is_empty());
    assert!(app.ui_ephemeral.status_message.is_empty(), "{}", app.ui_ephemeral.status_message);

    // 次の event の終わりで積み直す (何もしない event でよい)。
    app.handle_event(AppEvent::DirtyGuardCancel);
    assert_eq!(requests(&drain(&mut plugin_rx)), 1);
    respond(&mut app, device_id, &[1]);
    app.cur.song_doc.mark_saved();
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "基準があるので、閉じる前に比べる");
}

#[test]
fn タブを閉じるときも比べ_違えばそのタブで聞く() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);
    let key = app.cur.key;

    app.handle_event(AppEvent::Tab(TabEvent::Close(key)));
    assert!(app.tab(key).is_some(), "応答までは閉じない");
    respond(&mut app, device_id, &[2]);
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![key])));
    assert!(app.tab(key).is_some());
}

/// 確認モーダルの「保存」「保存しない」はアクティブなタブに効く。比較を待つ間に別のタブへ移っていたら、
/// 閉じようとしたタブへ戻してから聞く (移った先のタブを保存 / 破棄して、元のタブを黙って閉じない)。
#[test]
fn 比較を待つ間に別のタブへ移っても_閉じようとしたタブについて聞く() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);
    let closing = app.cur.key;
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let other = app.cur.key;
    assert_ne!(closing, other);

    app.handle_event(AppEvent::Tab(TabEvent::Close(closing)));
    app.handle_event(AppEvent::Tab(TabEvent::Switch(other)));
    assert_eq!(app.cur.key, other);
    respond_to(&mut app, closing, device_id, &[2]);
    assert_eq!(app.cur.key, closing, "閉じようとしたタブがアクティブ");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::CloseTabs(vec![closing])));

    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert!(app.tab(closing).is_none() && app.tab(other).is_some());
}

/// host が応答しない: 確かめられなかったので未保存に倒して聞く。「保存せず終了」で必ず抜けられる。
#[test]
fn 比較の応答が来なければ未保存に倒して聞き_保存せず終了で抜けられる() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, _device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    app.poll_state_roundtrip_watchdog(Instant::now() + Duration::from_secs(120));
    assert!(app.cur.pipc.pending_state_queue.is_empty());
    assert!(!app.shutdown.is_shutting_down(), "黙って終了しない");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));

    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert!(app.shutdown.is_shutting_down());
}

/// 待ちきれずに破棄した要求の応答が後から届いても、次の要求 (ここでは「保存して終了」の保存) の応答として
/// 受けない。受けると、破棄から保存までの間に変わった中身 (device の増減も) が入っていない古い時点の state を
/// 保存して終了してしまう。
#[test]
fn 破棄した要求の遅れた応答を_次の保存の応答として受けない() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");
    let (mut app, mut plugin_rx, device_id) = saved_project_with_plugin(&path, &[1]);
    let project = app.pk();

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    let stale = last_request(&drain(&mut plugin_rx));
    app.poll_state_roundtrip_watchdog(Instant::now() + Duration::from_secs(120));
    app.handle_event(AppEvent::DirtyGuardSave);
    let current = last_request(&drain(&mut plugin_rx));
    assert_ne!(stale, current, "要求ごとに別の番号");

    respond_as(&mut app, project, stale, device_id, &[9]);
    assert!(!app.shutdown.is_shutting_down() && app.cur.song_doc.is_dirty(), "古い応答では保存しない");
    assert_eq!(app.cur.pipc.pending_state_queue.len(), 1, "保存はまだ応答を待っている");

    respond_as(&mut app, project, current, device_id, &[2]);
    assert!(app.shutdown.is_shutting_down(), "{}", app.ui_ephemeral.status_message);
    let saved = common::project::load_project(&path).expect("reload");
    let state = saved.song.tracks[0].plugins().next().expect("synth").state.clone();
    assert_eq!(state.as_deref(), Some(&[2u8][..]), "保存の要求に対する応答の state が入っている");
}

/// host が居ない (crash 後に respawn を断念): 取り直せないので比べずに終了する (保存もできない状態)。
#[test]
fn host_が居なければ比べずに終了する() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _plugin_rx, _device_id) = saved_project_with_plugin(&dir.path().join("proj.daw"), &[1]);
    app.ipc.plugin_tx = None;
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert!(app.shutdown.is_shutting_down());
}

/// autosave は live の Song を書く。plugin の窓の中の変更は Song の blob にまだ無いので、先に取り直す。
#[test]
fn autosave_は報告された変更の_state_を取り直してから書く() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");
    let (mut app, mut plugin_rx, device_id) = saved_project_with_plugin(&path, &[1]);
    let sidecar = common::recovery::sidecar_for(&path);

    plugin_edited(&mut app, device_id);
    app.cur.song_doc.last_autosave = Instant::now().checked_sub(Duration::from_secs(61)).expect("uptime > 61s");
    app.maybe_autosave();
    app.maybe_autosave();
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "取り直しは 1 回だけ積む");
    assert!(!sidecar.exists(), "取り直す前の state では書かない");

    respond(&mut app, device_id, &[7]);
    let autosaved = common::project::load_project(&sidecar).expect("autosave written");
    let state = autosaved.song.tracks[0].plugins().next().expect("synth").state.clone();
    assert_eq!(state.as_deref(), Some(&[7u8][..]));
    assert!(app.cur.song_doc.is_dirty(), "autosave は未保存の印を降ろさない");
}

/// autosave の取り直しを待つ間に保存が済んだら、書かない (保存済みの文書に、ファイルより新しい autosave を
/// 残すと、次の起動で復元を聞かれる)。
#[test]
fn autosave_の取り直しの前に保存が済んだら書かない() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");
    let (mut app, _plugin_rx, device_id) = saved_project_with_plugin(&path, &[1]);
    let sidecar = common::recovery::sidecar_for(&path);

    plugin_edited(&mut app, device_id);
    app.handle_event(AppEvent::Save);
    app.cur.song_doc.last_autosave = Instant::now().checked_sub(Duration::from_secs(61)).expect("uptime > 61s");
    app.maybe_autosave();
    respond(&mut app, device_id, &[2]);
    assert!(!app.cur.song_doc.is_dirty(), "{}", app.ui_ephemeral.status_message);
    respond(&mut app, device_id, &[2]);
    assert!(app.cur.pipc.pending_state_queue.is_empty());
    assert!(!sidecar.exists());
}
