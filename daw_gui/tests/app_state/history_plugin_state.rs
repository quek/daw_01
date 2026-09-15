//! 履歴ジャンプ (undo / redo / 履歴リストの行) で plugin を host から降ろすときも、**降ろす前に最新の state を取り寄せる**
//! (`handler::history`)。取り寄せた state は live と履歴の全 Song に書き戻すので、次に同じ device を載せ直すどの経路
//! (redo / undo / 履歴ジャンプ / 再有効化) も回した後の値で載る。降ろす device の無いジャンプは同期のまま。
//!
//! ここが崩れると「有効に戻す → ツマミを回す → Ctrl+Z (無効へ) → 次に有効にすると回す前の音」になる。

use std::time::{Duration, Instant};

use common::protocol::{PluginCommand, PluginEvent, SlotState};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_device::DeviceEvent;

use super::support::{build_app, drain, fake_plugin_loaded, load_instrument};

/// plugin host の `RequestAllStates` 応答を模す (`device_id` の今の state = `data`)。
fn respond(app: &mut AppData, device_id: u64, data: &[u8]) {
    let entries = vec![SlotState { device_id, data: Some(data.to_vec()), ara_archive: None, error: None }];
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries }));
}

fn requests(msgs: &[PluginCommand]) -> usize {
    msgs.iter().filter(|m| matches!(m, PluginCommand::RequestAllStates { .. })).count()
}

fn removes(msgs: &[PluginCommand], device_id: u64) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, PluginCommand::RemoveSlotPlugin { device } if device.device_id == device_id))
        .count()
}

fn loaded_states(msgs: &[PluginCommand], device_id: u64) -> Vec<Option<Vec<u8>>> {
    msgs.iter()
        .filter_map(|m| match m {
            PluginCommand::SetSlotPlugin { device, initial_state, .. } if device.device_id == device_id => {
                Some(initial_state.clone())
            }
            _ => None,
        })
        .collect()
}

/// track 0 に楽器を載せて `(track_id, device_id)`。
fn instrument(app: &mut AppData) -> (u32, u64) {
    load_instrument(app);
    let track = &app.cur.song_doc.song().tracks[0];
    (track.id, track.plugins().next().expect("synth").id)
}

fn set_volume(app: &mut AppData, amp: f32) {
    let track = app.cur.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::SetTrackVolume { track, amp });
}

#[test]
fn 有効に戻して回したツマミは_無効へ戻す_undo_で取り寄せられ_redo_と再有効化で載る() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let (track_id, device_id) = instrument(&mut app);
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: false });
    respond(&mut app, device_id, &[1]);
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: true });
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth");
    drain(&mut plugin_rx);

    // ツマミを回した (host の state は [2]) → Ctrl+Z で無効へ戻る: 取り寄せてから降ろす。
    app.handle_event(AppEvent::Undo);
    let msgs = drain(&mut plugin_rx);
    assert_eq!((requests(&msgs), removes(&msgs, device_id)), (1, 0), "取り寄せる前に降ろさない: {msgs:?}");
    assert!(app.cur.song_doc.song().track_effectively_enabled(track_id), "取り寄せの間は反映しない");
    respond(&mut app, device_id, &[2]);
    assert!(!app.cur.song_doc.song().track_effectively_enabled(track_id));
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 1);

    // redo (有効へ) は降ろさないので即時、回した後の値で載る。
    app.handle_event(AppEvent::Redo);
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![2])]);
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth");

    // もう一度回して ([3]) 無効へ戻し、今度は undo した先で有効にし直す (新しい編集) — これも回した後の値で載る。
    app.handle_event(AppEvent::Undo);
    respond(&mut app, device_id, &[3]);
    drain(&mut plugin_rx);
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: true });
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![3])]);
}

#[test]
fn 削除を戻して回したツマミは_redo_で取り寄せられ_次の_undo_で載る() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let (track_id, device_id) = instrument(&mut app);
    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![device_id] }));
    respond(&mut app, device_id, &[1]);
    drain(&mut plugin_rx);
    app.handle_event(AppEvent::Undo);
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![1])], "載せる undo は即時");
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth");

    // ツマミを回した ([5]) → redo (削除) は取り寄せてから降ろす。
    app.handle_event(AppEvent::Redo);
    let msgs = drain(&mut plugin_rx);
    assert_eq!((requests(&msgs), removes(&msgs, device_id)), (1, 0), "{msgs:?}");
    assert!(app.cur.song_doc.song().plugin_by_id(device_id).is_some(), "取り寄せの間は反映しない");
    respond(&mut app, device_id, &[5]);
    assert!(app.cur.song_doc.song().plugin_by_id(device_id).is_none());
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 1);

    app.handle_event(AppEvent::Undo);
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![5])]);
}

/// 履歴リストで plugin の無い state まで一気に戻り、途中の state (plugin を足した直後) へ進んでも、回した後の値で載る
/// (取り寄せた state は live だけでなく履歴の全 Song に書き戻す)。
#[test]
fn 履歴を跨いで戻り途中の_state_へ進んでも回した後の値で載る() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let before_plugin = app.cur.song_doc.history_current();
    let (track_id, device_id) = instrument(&mut app);
    let with_plugin = app.cur.song_doc.history_current();
    set_volume(&mut app, 0.5);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::JumpHistory(before_plugin));
    assert_eq!(app.cur.song_doc.history_current(), with_plugin + 1, "取り寄せの間は反映しない");
    respond(&mut app, device_id, &[4]);
    assert_eq!(app.cur.song_doc.history_current(), before_plugin);
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 1);

    app.handle_event(AppEvent::JumpHistory(with_plugin));
    assert_eq!(app.cur.song_doc.history_current(), with_plugin, "載せるジャンプは即時");
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![4])]);
    let _ = track_id;
}

/// 取り寄せ待ちの間に来た undo は順番に処理する (取り寄せ → ジャンプ → 次の取り寄せ → ジャンプ)。
#[test]
fn 取り寄せ待ちの間の_undo_連打は順番に処理する() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let base_volume = app.cur.song_doc.song().tracks[0].volume;
    set_volume(&mut app, 0.25);
    let volume_step = app.cur.song_doc.history_current();
    let (_track_id, device_id) = instrument(&mut app);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::Undo);
    app.handle_event(AppEvent::Undo);
    assert_eq!(requests(&drain(&mut plugin_rx)), 1, "取り寄せ中は次を送らない");
    assert_eq!(app.cur.pipc.pending_state_queue.len(), 2);

    respond(&mut app, device_id, &[6]);
    let msgs = drain(&mut plugin_rx);
    assert_eq!(app.cur.song_doc.history_current(), volume_step, "1 回目の undo (plugin を外す) だけ");
    assert_eq!((removes(&msgs, device_id), requests(&msgs)), (1, 1), "2 回目の取り寄せを送る: {msgs:?}");
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, 0.25);

    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    assert_eq!(app.cur.song_doc.history_current(), volume_step - 1, "2 回目の undo (音量)");
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, base_volume);
    assert!(app.cur.pipc.pending_state_queue.is_empty());
}

/// 取り寄せ待ちの間に入った編集 (往復を待たない編集はその場で入る) で、待っている undo の行き先はずれない:
/// 押した時点の「1 段前」へ動き、間に入った編集は redo 側に回る。
#[test]
fn 取り寄せ待ちの間に入った編集で_undo_の行き先はずれない() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let base_volume = app.cur.song_doc.song().tracks[0].volume;
    let before_plugin = app.cur.song_doc.history_current();
    let (_track_id, device_id) = instrument(&mut app);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::Undo);
    set_volume(&mut app, 0.5);
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, 0.5, "待たない編集はその場で入る");
    respond(&mut app, device_id, &[8]);

    let doc = &app.cur.song_doc;
    assert_eq!(doc.history_current(), before_plugin, "押した時点の 1 段前 (plugin を足す前)");
    assert!(doc.song().plugin_by_id(device_id).is_none());
    assert_eq!(doc.song().tracks[0].volume, base_volume);
    assert_eq!(doc.history_labels().len(), before_plugin + 3, "間に入った編集は redo 側に残る");
}

/// 往復待ちの削除の後に来た undo は、削除が済んでから動く = 削除を戻す (先に動くとその前の操作が戻る)。
#[test]
fn 往復待ちの削除の後に来た_undo_は削除を戻す() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    set_volume(&mut app, 0.25);
    let (_track_id, device_id) = instrument(&mut app);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![device_id] }));
    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.pipc.pending_state_queue.len(), 2, "undo は削除の後ろに並ぶ");
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, 0.25, "並んでいる間は何も戻さない");

    respond(&mut app, device_id, &[7]);
    assert!(app.cur.song_doc.song().plugin_by_id(device_id).is_none(), "削除が先");
    // undo の取り寄せ (降ろした device はもう host に居ないので state は来ない)。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    let restored = app.cur.song_doc.song().plugin_by_id(device_id).expect("undo で削除が戻る");
    assert_eq!(restored.state.as_deref(), Some(&[7_u8][..]));
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, 0.25, "その前の操作は戻らない");
    assert_eq!(loaded_states(&drain(&mut plugin_rx), device_id), vec![Some(vec![7])]);
}

/// 降ろす device の無い undo は往復を挟まず即時。
#[test]
fn 降ろす_device_の無い_undo_は同期のまま() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    instrument(&mut app);
    let before = app.cur.song_doc.song().tracks[0].volume;
    set_volume(&mut app, 0.25);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.song_doc.song().tracks[0].volume, before);
    assert_eq!(requests(&drain(&mut plugin_rx)), 0);
    assert!(app.cur.pipc.pending_state_queue.is_empty());
}

/// 取り寄せが終わらない (host の hang) と、往復待ちの編集と同じくジャンプは適用しない (state を取れないまま降ろさない)。
#[test]
fn 取り寄せが終わらなければ_undo_は適用しない() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let (_track_id, device_id) = instrument(&mut app);
    let current = app.cur.song_doc.history_current();
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::Undo);
    app.poll_state_roundtrip_watchdog(Instant::now() + Duration::from_secs(120));
    assert!(app.cur.pipc.pending_state_queue.is_empty());
    assert_eq!(app.cur.song_doc.history_current(), current, "undo は適用しない");
    assert!(app.cur.song_doc.song().plugin_by_id(device_id).is_some());
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 0);
}
