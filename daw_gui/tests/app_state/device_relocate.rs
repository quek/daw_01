//! r.md #71 (プラグインのコピー / 移動) の headless テスト。
//!
//! 検証する不変量:
//! - **移動は instance を作り直さない** (device_id が不変 = 音が切れない)
//! - automation lane と mod routing は device と一緒に運ばれ、**lane id は
//!   移送先で再採番される** (据え置くと dest 側の既存 lane と衝突して、選択や
//!   行高 override が silent に別 lane へ付け替わる)
//! - **コピーは新 id を採番し、state だけ引き継ぐ** (automation / 変調は複製しない)
//! - ARA アーカイブはトラックを跨いだら捨てる (persistent_id が元トラックの
//!   クリップを指すので復元できない)
//! - VOICEVOX / Transform の「トラックに付く印」は device の実在に追従する
//! - **移動後に元トラックを削除しても移動先の device は teardown されない**
//!   (= `[[project_plugin_slot_rekey]]` の再発防止)
//! - device 選択は「いま表示しているチェーン」にスコープされる

use common::model::{
    AutomationLane, AutomationLaneKey, AutomationTarget, InstrumentSource, PluginInstance,
};
use common::plugin_format::PluginFormat;
use common::protocol::PluginCommand;

use daw_gui::app::{AppData, AppEvent, EditSurface, RelocateDevices};
use daw_gui::widgets::select_modifier::SelectModifier;

use super::support::{build_app, drain, fake_plugin_loaded, select_track_single};

/// 空の追加トラックを 1 本作って id を返す。
fn add_empty_track(app: &mut AppData) -> u32 {
    app.edit_song(|song| {
        let id = song.alloc_track_id();
        let mut t = common::model::Track {
            id,
            ..common::model::Track::default()
        };
        t.name = format!("T{id}");
        song.tracks.push(t);
        id
    })
    .expect("edit_song")
}

/// `track_id` のチェーン末尾に plugin を 1 個 picker 経由で足し、load 応答まで
/// fake する。 戻り値は device_id。
fn add_plugin(app: &mut AppData, track_id: u32, plugin_id: &str) -> u64 {
    let idx = app
        .song_doc
        .song()
        .tracks
        .iter()
        .position(|t| t.id == track_id)
        .expect("track exists");
    select_track_single(app, idx);
    let at = app.song_doc.song().tracks[idx].devices.len() as u32;
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: plugin_id.into(),
        keep_open: false,
        open_gui: false,
    });
    fake_plugin_loaded(app, track_id, at, plugin_id)
}

/// `device_id` を対象にした PluginParam lane を `track_id` に 1 本作る。
/// 戻り値は lane id。
fn add_plugin_param_lane(app: &mut AppData, track_id: u32, device_id: u64) -> u32 {
    app.edit_song(|song| {
        let t = song
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id)
            .expect("track exists");
        let id = t.alloc_lane_id();
        let mut lane = AutomationLane::new(
            AutomationTarget::PluginParam {
                device_id,
                param_id: 7,
                legacy_device_index: None,
            },
            0.5,
        );
        lane.id = id;
        t.automation_lanes.push(lane);
        id
    })
    .expect("edit_song")
}

fn track_devices(app: &AppData, track_id: u32) -> Vec<u64> {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .map(|t| t.devices.iter().map(|d| d.id).collect())
        .unwrap_or_default()
}

/// 移動: device_id は不変、automation lane も一緒に運ばれ、lane id は
/// 移送先で再採番される。
#[test]
fn move_between_tracks_keeps_device_id_and_carries_lane() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");
    let old_lane = add_plugin_param_lane(&mut app, t0, dev);
    // 移送先にも別 lane を 1 本置いて、id 衝突が起きうる状況を作る。
    let dest_existing = app
        .edit_song(|song| {
            let t = song.tracks.iter_mut().find(|t| t.id == t1).unwrap();
            let id = t.alloc_lane_id();
            let mut lane = AutomationLane::new(
                AutomationTarget::TrackBuiltin(common::model::TrackBuiltinParam::Volume),
                0.5,
            );
            lane.id = id;
            t.automation_lanes.push(lane);
            id
        })
        .expect("edit_song");
    let _ = drain(&mut plugin_rx);

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: false,
    }));
    // plugin state の round-trip 待ちに積まれるので、応答を fake して実行させる。
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    assert!(track_devices(&app, t0).is_empty(), "元トラックから消える");
    assert_eq!(track_devices(&app, t1), vec![dev], "device_id は不変のまま移る");

    let t0_lanes = &app
        .song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == t0)
        .unwrap()
        .automation_lanes;
    assert!(
        t0_lanes.is_empty(),
        "PluginParam lane は元トラックに残らない (残すと永久に効かない)"
    );
    let t1_lanes = &app
        .song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == t1)
        .unwrap()
        .automation_lanes;
    let moved = t1_lanes
        .iter()
        .find(|l| {
            matches!(l.target, AutomationTarget::PluginParam { device_id, .. } if device_id == dev)
        })
        .expect("PluginParam lane が移送先にある");
    assert_ne!(moved.id, dest_existing, "lane id は dest 側の既存 lane と衝突しない");
    assert_ne!(
        moved.id, old_lane,
        "lane id は据え置かず移送先の allocator で採番し直す"
    );

    // 移動は plugin_host への IPC を 1 通も出さない (instance を作り直さない)。
    let msgs = drain(&mut plugin_rx);
    assert!(
        !msgs.iter().any(|m| matches!(
            m,
            PluginCommand::RemoveSlotPlugin { .. } | PluginCommand::SetSlotPlugin { .. }
        )),
        "移動で instance は作り直さない: {msgs:?}"
    );
}

/// lane 移送に伴い、session-only の行高 override も新しい鍵へ写し替わる
/// (写さないと「行高だけ元の位置に取り残されて別 lane に化ける」)。
#[test]
fn move_across_tracks_rekeys_lane_row_override() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");
    let lane = add_plugin_param_lane(&mut app, t0, dev);
    let from = AutomationLaneKey { track: t0, lane };
    app.ui_prefs.automation_lane_row_overrides.insert(from, 123);

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    let new_lane = app
        .song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == t1)
        .unwrap()
        .automation_lanes
        .iter()
        .find(|l| {
            matches!(l.target, AutomationTarget::PluginParam { device_id, .. } if device_id == dev)
        })
        .expect("moved lane")
        .id;
    let to = AutomationLaneKey { track: t1, lane: new_lane };
    assert!(
        !app.ui_prefs.automation_lane_row_overrides.contains_key(&from),
        "旧キーは消える"
    );
    assert_eq!(
        app.ui_prefs.automation_lane_row_overrides.get(&to).copied(),
        Some(123),
        "新キーへ写る"
    );
}

/// ARA アーカイブはトラックを跨いだら捨てる (同一トラック内の移動では残る)。
#[test]
fn move_across_tracks_drops_ara_archive() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");
    let other = add_plugin(&mut app, t0, "test.delay");
    app.edit_song(|song| {
        let d = daw_gui::app::device_mut_by_id(song, dev).unwrap();
        d.ara_archive = Some(std::sync::Arc::from(&b"melodyne"[..]));
    });

    // (1) 同一チェーン内の移動 (= 並べ替え) では残る。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t0,
        dest_index: 2,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    assert_eq!(track_devices(&app, t0), vec![other, dev], "同一チェーン内で並べ替わる");
    let song = app.song_doc.song();
    let (tr, idx) = daw_gui::app::find_device_by_id(song, dev).unwrap();
    assert!(
        daw_gui::app::device_at(song, tr, idx)
            .unwrap()
            .ara_archive
            .is_some(),
        "同一トラック内では ARA アーカイブが残る"
    );

    // (2) トラックを跨いだら捨てる。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    let song = app.song_doc.song();
    let (tr, idx) = daw_gui::app::find_device_by_id(song, dev).unwrap();
    assert_eq!(tr, t1);
    assert!(
        daw_gui::app::device_at(song, tr, idx)
            .unwrap()
            .ara_archive
            .is_none(),
        "トラックを跨いだら ARA アーカイブは捨てる (解析し直す)"
    );
}

/// コピー: 新しい device id が振られ、state は引き継ぐが automation / 変調は
/// 複製しない。 新 device は host へ実体化される。
#[test]
fn copy_allocates_new_id_and_keeps_state() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");
    add_plugin_param_lane(&mut app, t0, dev);
    app.edit_song(|song| {
        let d = daw_gui::app::device_mut_by_id(song, dev).unwrap();
        d.state = Some(std::sync::Arc::from(&b"abc"[..]));
    });
    let _ = drain(&mut plugin_rx);

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: true,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    assert_eq!(track_devices(&app, t0), vec![dev], "コピー元は残る");
    let copies = track_devices(&app, t1);
    assert_eq!(copies.len(), 1);
    let new_id = copies[0];
    assert_ne!(new_id, dev, "コピーは新 id を採番する");

    let song = app.song_doc.song();
    let (tr, idx) = daw_gui::app::find_device_by_id(song, new_id).unwrap();
    assert_eq!(
        daw_gui::app::device_at(song, tr, idx)
            .unwrap()
            .state
            .as_deref(),
        Some(&b"abc"[..]),
        "ツマミの現在値 (state) は引き継ぐ"
    );
    assert!(
        song.tracks
            .iter()
            .find(|t| t.id == t1)
            .unwrap()
            .automation_lanes
            .is_empty(),
        "automation lane は複製しない (確定方針)"
    );
    assert!(
        app.ipc.pending_added_plugin_finalize.contains_key(&new_id),
        "コピーした device は load 完了 finalize に積まれる"
    );
    let msgs = drain(&mut plugin_rx);
    assert!(
        msgs.iter().any(
            |m| matches!(m, PluginCommand::SetSlotPlugin { device_id, .. } if *device_id == new_id)
        ),
        "コピーは新 instance を host に作らせる: {msgs:?}"
    );
}

/// コピーの ARA: 別トラックへなら捨て、同一トラック内なら引き継ぐ。
#[test]
fn copy_to_other_track_drops_ara_but_same_track_keeps_it() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");
    app.edit_song(|song| {
        let d = daw_gui::app::device_mut_by_id(song, dev).unwrap();
        d.ara_archive = Some(std::sync::Arc::from(&b"melodyne"[..]));
    });

    // 同一トラック内のコピー → 引き継ぐ。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t0,
        dest_index: 1,
        copy: true,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    let same_copy = track_devices(&app, t0)
        .into_iter()
        .find(|id| *id != dev)
        .expect("same-track copy");
    {
        let song = app.song_doc.song();
        let (tr, idx) = daw_gui::app::find_device_by_id(song, same_copy).unwrap();
        assert!(
            daw_gui::app::device_at(song, tr, idx)
                .unwrap()
                .ara_archive
                .is_some(),
            "同一トラックへのコピーは ARA アーカイブを引き継ぐ"
        );
    }

    // 別トラックへのコピー → 捨てる。
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: true,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    let cross_copy = track_devices(&app, t1)[0];
    let song = app.song_doc.song();
    let (tr, idx) = daw_gui::app::find_device_by_id(song, cross_copy).unwrap();
    assert!(
        daw_gui::app::device_at(song, tr, idx)
            .unwrap()
            .ara_archive
            .is_none(),
        "別トラックへのコピーは ARA アーカイブを捨てる"
    );
}

/// VOICEVOX builtin を移すと「歌う印」も一緒に移る (src から降り、dest に立つ)。
#[test]
fn move_voicevox_moves_vocal_marker() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    // builtin は plugin_db に無いので直接 Song へ入れる (picker 経由と同じ形)。
    let dev = app
        .edit_song(|song| {
            let id = song.alloc_device_id();
            let t = song.tracks.iter_mut().find(|t| t.id == t0).unwrap();
            t.devices.push(PluginInstance {
                id,
                ..PluginInstance::new(
                    common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                    PluginFormat::Builtin,
                )
            });
            t.source = InstrumentSource::Vocal;
            id
        })
        .expect("edit_song");

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    let src = app.song_doc.song().tracks.iter().find(|t| t.id == t0).unwrap();
    let dst = app.song_doc.song().tracks.iter().find(|t| t.id == t1).unwrap();
    assert_eq!(src.source, InstrumentSource::None, "元トラックの印は降りる");
    assert_eq!(dst.source, InstrumentSource::Vocal, "移送先に印が立つ");
}

/// VOICEVOX を 2 本持つトラックで 1 本だけ消しても印は残る
/// (削除側にも「他に残っていなければ」ガードを入れて Transform と対称にした)。
#[test]
fn removing_one_of_two_voicevox_keeps_vocal_marker() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let (a, _b) = app
        .edit_song(|song| {
            let a = song.alloc_device_id();
            let b = song.alloc_device_id();
            let t = song.tracks.iter_mut().find(|t| t.id == t0).unwrap();
            for id in [a, b] {
                t.devices.push(PluginInstance {
                    id,
                    ..PluginInstance::new(
                        common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                        PluginFormat::Builtin,
                    )
                });
            }
            t.source = InstrumentSource::Vocal;
            (a, b)
        })
        .expect("edit_song");

    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![a] });
    // 削除は plugin state の round-trip 待ちに積まれるので、応答を fake して実行させる。
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    let src = app.song_doc.song().tracks.iter().find(|t| t.id == t0).unwrap();
    assert_eq!(src.devices.len(), 1, "1 本だけ消える");
    assert_eq!(
        src.source,
        InstrumentSource::Vocal,
        "もう 1 本残っているので印は保つ"
    );
}

/// **移動後に元トラックを削除しても、移動先の device は teardown されない**
/// (= `[[project_plugin_slot_rekey]]` の再発防止)。
#[test]
fn deleting_source_track_after_move_keeps_moved_device_loaded() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev = add_plugin(&mut app, t0, "test.fx");

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t1,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    // plan は「Song から列挙する」ので、移動した device は元トラックの plan に出ない。
    let plan = AppData::plan_track_removal_ipc(app.song_doc.song(), &[t0]);
    assert!(
        plan.is_empty(),
        "移動で空になった元トラックの teardown plan は空: {plan:?}"
    );

    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::DeleteTracks(vec![t0]));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    let msgs = drain(&mut plugin_rx);
    assert!(
        !msgs.iter().any(
            |m| matches!(m, PluginCommand::RemoveSlotPlugin { device_id } if *device_id == dev)
        ),
        "移動先の device は元トラック削除で teardown されない: {msgs:?}"
    );
    assert_eq!(track_devices(&app, t1), vec![dev], "移動先に残ったまま");
}

/// track → master → track の往復で device id / lane / 副作用が整合する。
#[test]
fn master_chain_round_trip() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let dev = add_plugin(&mut app, t0, "test.fx");
    add_plugin_param_lane(&mut app, t0, dev);
    let master = common::model::MASTER_TRACK_ID;

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: master,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    assert_eq!(
        app.song_doc.song().master_fx_chain.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![dev],
        "master へ移る"
    );
    assert_eq!(
        app.song_doc.song().song_lanes.len(),
        1,
        "lane は song_lanes へ移る (master は song 所有)"
    );

    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![dev],
        dest_track: t0,
        dest_index: 0,
        copy: false,
    }));
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));
    assert!(app.song_doc.song().master_fx_chain.is_empty(), "master から戻る");
    assert!(app.song_doc.song().song_lanes.is_empty(), "lane も戻る");
    assert_eq!(track_devices(&app, t0), vec![dev], "device_id は往復しても不変");
    assert_eq!(
        app.song_doc
            .song()
            .tracks
            .iter()
            .find(|t| t.id == t0)
            .unwrap()
            .automation_lanes
            .len(),
        1,
        "lane が track へ戻る"
    );
}

/// 貼り付け位置は「選んでいるプラグインの直前」、無選択なら末尾 (Ableton 流)。
#[test]
fn paste_devices_inserts_before_selection() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let a = add_plugin(&mut app, t0, "test.fx");
    let b = add_plugin(&mut app, t0, "test.delay");
    let c = add_plugin(&mut app, t0, "test.bitcrush");

    let payload = || {
        vec![daw_gui::clipboard::DeviceCopy {
            order: 0,
            source_track: t0,
            device: PluginInstance::new("test.fx".into(), PluginFormat::Clap),
        }]
    };

    // 2 番目 (b) を選択 → 挿入位置は index 1。
    app.set_device_selection(vec![b]);
    assert_eq!(app.paste_devices(payload(), t0), 1);
    let after = track_devices(&app, t0);
    assert_eq!(after.len(), 4);
    assert_eq!(after[0], a);
    assert_eq!(after[2], b, "b の直前に入る");
    assert_eq!(after[3], c);

    // 無選択 → 末尾。
    app.set_device_selection(Vec::new());
    assert_eq!(app.paste_devices(payload(), t0), 1);
    let after = track_devices(&app, t0);
    assert_eq!(after.len(), 5);
    assert_eq!(after[4], *track_devices(&app, t0).last().unwrap());
    assert_eq!(after[3], c, "末尾に足されるので c の位置は変わらない");
}

/// device 選択は「いま表示しているチェーン」にスコープされる。
/// cursor track が動いた時点で元トラックの id は stale になり、
/// **読む側の正規化** で落ちる (= 掃除を全 writer に挿す補償コードを持たない)。
#[test]
fn device_selection_is_scoped_to_displayed_chain() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let t0 = app.song_doc.song().tracks[0].id;
    let t1 = add_empty_track(&mut app);
    let dev0 = add_plugin(&mut app, t0, "test.fx");
    let dev1 = add_plugin(&mut app, t1, "test.delay");

    // t0 の device を選ぶ → タグは Devices。
    select_track_single(&mut app, 0);
    app.handle_event(AppEvent::SelectDevice {
        device_id: dev0,
        modifier: SelectModifier::Single,
    });
    assert_eq!(app.edit_surface(false), Some(EditSurface::Devices));
    assert_eq!(app.live_device_ids(), vec![dev0]);

    // カーソルトラックだけ動かす (選択集合は触らない = ドラッグ途中の状態)。
    let idx1 = app
        .song_doc
        .song()
        .tracks
        .iter()
        .position(|t| t.id == t1)
        .unwrap();
    select_track_single(&mut app, idx1);
    assert!(
        app.live_device_ids().is_empty(),
        "表示チェーンに居ない id は正規化で落ちる"
    );
    assert_ne!(
        app.edit_surface(false),
        Some(EditSurface::Devices),
        "device 面は空なので対象にならない (= 次の Delete が画面外の device を消さない)"
    );

    // t1 の device を Ctrl+click → 異トラックの id は混ざらない。
    app.handle_event(AppEvent::SelectDevice {
        device_id: dev1,
        modifier: SelectModifier::Toggle,
    });
    assert_eq!(app.live_device_ids(), vec![dev1]);
    assert!(
        !app.selection.selected_device_ids.contains(&dev0),
        "異トラックの id は選択集合に残らない"
    );
}
