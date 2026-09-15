//! picker で選んだ device の挿し先。トラックが 0 本の曲でも「追加」が空のトラックだけ残して消えないこと、master 宛てに
//! 余計なトラックを足さないこと、閉じた picker の挿し先を後から使わないこと (`AppData::plugin_insert_chain`)。

use common::model::{ChainRef, Device, MASTER_TRACK_ID, NativeKind};
use common::plugin_db::{NATIVE_COMP_PICKER_ID, NATIVE_EQ_PICKER_ID, PARALLEL_PICKER_ID};
use common::protocol::PluginCommand;

use daw_gui::app::{AppData, AppEvent};

use super::support::{build_app, drain, select_track_single};

/// トラックが 1 本も無い曲 (cursor も無い)。
fn app_without_tracks() -> (AppData, tokio::sync::mpsc::UnboundedReceiver<PluginCommand>) {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    app.edit_song(|song| song.tracks.clear()).expect("edit");
    app.cur.selection.selected_track_ids.clear();
    drain(&mut plugin_rx);
    (app, plugin_rx)
}

fn pick(app: &mut AppData, chain: Option<ChainRef>, id: &str) {
    app.handle_event(AppEvent::OpenPluginPicker { chain });
    app.handle_event(AppEvent::SelectPluginFromDb { id: id.into(), keep_open: false, open_gui: false });
}

/// 追加分 (組み込みでない) の内蔵 `kind` が `devices` の最上位に居るか。
fn has_added_native(devices: &[Device], kind: NativeKind) -> bool {
    devices.iter().any(|d| matches!(d, Device::Native(n) if !n.builtin && n.kind() == kind))
}

/// トラックが 0 本の曲で内蔵 / Parallel / plugin を選ぶと、足したトラックに入り cursor もそこへ移る。トラックと device は
/// 1 回の操作なので undo 1 回で両方戻る。
#[test]
fn トラックが無い曲で選んだ_device_は足したトラックに入り_undo_1_回で両方戻る() {
    type Check = fn(&[Device]) -> bool;
    let cases: [(&str, Check); 3] = [
        (NATIVE_COMP_PICKER_ID, |d| has_added_native(d, NativeKind::Comp)),
        (PARALLEL_PICKER_ID, |d| d.iter().any(|d| matches!(d, Device::Parallel(_)))),
        ("test.synth", |d| common::model::plugins(d).any(|p| p.plugin_id == "test.synth")),
    ];
    for (id, inserted) in cases {
        let (mut app, mut plugin_rx) = app_without_tracks();
        let depth = app.cur.song_doc.undo_depth();

        pick(&mut app, None, id);
        let song = app.cur.song_doc.song();
        assert_eq!(song.tracks.len(), 1, "{id}: トラックを 1 本足す");
        let track = &song.tracks[0];
        assert!(inserted(&track.devices), "{id}: 足したトラックに挿す: {:?}", track.devices);
        assert_eq!(app.cursor_track_id(), Some(track.id), "{id}: cursor は足したトラック");
        assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "{id}: 1 操作 = 1 undo step");
        if id == "test.synth" {
            let msgs = drain(&mut plugin_rx);
            assert!(msgs.iter().any(|m| matches!(m, PluginCommand::SetSlotPlugin { .. })), "plugin は host に載せる");
        }

        app.handle_event(AppEvent::Undo);
        assert!(app.cur.song_doc.song().tracks.is_empty(), "{id}: undo 1 回でトラックごと戻る");
    }
}

/// master 宛て (master の chain で picker を開いた / cursor が master) なら、トラックが 0 本でもトラックを足さずに master へ挿す。
#[test]
fn master_宛てはトラックが無くてもトラックを足さずに_master_へ挿す() {
    let (mut app, _plugin_rx) = app_without_tracks();
    pick(&mut app, Some(ChainRef::Track(MASTER_TRACK_ID)), "test.fx");
    let song = app.cur.song_doc.song();
    assert!(song.tracks.is_empty(), "master 宛てで空のトラックを足した");
    assert!(common::model::plugins(&song.master_fx_chain).any(|p| p.plugin_id == "test.fx"));

    app.cur.selection.selected_track_ids = vec![MASTER_TRACK_ID];
    pick(&mut app, None, NATIVE_EQ_PICKER_ID);
    let song = app.cur.song_doc.song();
    assert!(song.tracks.is_empty(), "cursor が master のときに空のトラックを足した");
    assert!(has_added_native(&song.master_fx_chain, NativeKind::Eq));
}

/// 閉じた picker の挿し先は使わない: picker を閉じた後に同じ口で足す (インスペクタの「+ 字幕デバイス」) と cursor の
/// トラックに入る (前に picker を開いた master には入らない)。
#[test]
fn 閉じた_picker_の挿し先には挿さず_cursor_のトラックへ挿す() {
    let (mut app, _audio_rx, _plugin_rx, _d) = build_app();
    app.handle_event(AppEvent::OpenPluginPicker { chain: Some(ChainRef::Track(MASTER_TRACK_ID)) });
    app.handle_event(AppEvent::ClosePluginPicker);
    select_track_single(&mut app, 0);
    app.handle_event(AppEvent::SelectPluginFromDb { id: "test.fx".into(), keep_open: false, open_gui: false });
    let song = app.cur.song_doc.song();
    assert!(common::model::plugins(&song.tracks[0].devices).any(|p| p.plugin_id == "test.fx"), "cursor のトラックに挿す");
    assert!(!common::model::plugins(&song.master_fx_chain).any(|p| p.plugin_id == "test.fx"), "閉じた picker の挿し先に挿した");
}
