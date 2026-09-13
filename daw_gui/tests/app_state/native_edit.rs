//! r.md #129 (`docs/plan_rack_native_devices.md` §15.2 F-G2): 内蔵 device の編集ハブの headless 回帰。
//!
//! 旧 `tests/channel_strip_edit.rs` / `tests/master_strip_edit.rs` を統合した。見ているのは
//! 「値 IPC / undo / dirty / last touched / Listen / GR 表示 / 掃除」が **1 本の口** から出ること。

use common::model::{
    AutomationLane, AutomationTarget, ChainRef, CompParam, EqBand, EqParam, MASTER_TRACK_ID, ModRouting,
    ModSource, ModSourceKind, NativeKind, NativeParamId, Polarity, TrackBuiltinParam,
};
use common::protocol::AudioCommand;
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent, InsertAt};
use daw_gui::event::StripSection;
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_native::NativeEdit;

use super::support::{self, drain};

fn builtin(app: &AppData, owner: u32, kind: NativeKind) -> u64 {
    app.cur.song_doc.song().builtin_native(owner, kind).expect("組み込みが補充されている").id
}

fn native_edit(app: &mut AppData, device_id: u64, edit: NativeEdit) {
    app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit { device_id, edit }));
}

fn set_native_values(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<(u64, bool)> {
    drain(rx)
        .into_iter()
        .filter_map(|c| match c {
            AudioCommand::SetNativeDevice { device_id, bypassed, .. } => Some((device_id, bypassed)),
            _ => None,
        })
        .collect()
}

fn sc_listen_cmds(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<Option<u64>> {
    drain(rx)
        .into_iter()
        .filter_map(|c| match c {
            AudioCommand::SetScListen { device_id, .. } => Some(device_id),
            _ => None,
        })
        .collect()
}

fn thr() -> NativeParamId {
    NativeParamId::Comp(CompParam::Threshold)
}

/// ① 値 IPC は変化があったときだけ 1 通、undo 1 回で戻り、dirty が立つ。
#[test]
fn native_edit_sends_one_value_command_and_undoes_in_one_step() {
    let (mut app, mut audio_rx, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    let comp = builtin(&app, track, NativeKind::Comp);
    let _ = drain(&mut audio_rx);
    let depth = app.cur.song_doc.undo_depth();

    native_edit(&mut app, comp, NativeEdit::param(thr(), -18.0));
    assert_eq!(set_native_values(&mut audio_rx), vec![(comp, false)], "触ったら ON で 1 通");
    assert!(app.cur.song_doc.is_dirty());
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1);

    native_edit(&mut app, comp, NativeEdit::param(thr(), -18.0));
    assert!(set_native_values(&mut audio_rx).is_empty(), "同じ値は 0 通");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "変化の無い編集は undo を積まない");

    app.handle_event(AppEvent::Undo);
    let dev = app.cur.song_doc.song().native_by_id(comp).expect("comp");
    assert!(dev.bypassed, "undo で OFF に戻る");
    assert_ne!(dev.param(thr()), Some(-18.0));
}

/// ② 追加分への編集は組み込みを変えない / ③ master Bus Comp の last touched は MASTER。
#[test]
fn added_device_edits_stay_local_and_master_touches_are_owned_by_master() {
    let (mut app, _a, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    let builtin_comp = builtin(&app, track, NativeKind::Comp);
    let before = *app.cur.song_doc.song().native_by_id(builtin_comp).unwrap();
    app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
        chain: ChainRef::Track(track),
        kind: NativeKind::Comp,
        open_panel: true,
    }));
    let added = app.cur.selection.selected_device_ids[0];
    let dev = *app.cur.song_doc.song().native_by_id(added).expect("追加分");
    assert!(!dev.builtin && dev.ordinal == 2, "追加分は番号 2");
    assert!(app.cur.view.open_rack_panels.contains(&common::model::RackPanelKey::Device(added)));
    // 追加分は組み込みの手前 (Q6)。
    let devices = &app.cur.song_doc.song().tracks[0].devices;
    let pos = |id| devices.iter().position(|d| d.id() == id).unwrap();
    assert!(pos(added) < pos(builtin_comp));

    native_edit(&mut app, added, NativeEdit::param(thr(), -30.0));
    assert_eq!(*app.cur.song_doc.song().native_by_id(builtin_comp).unwrap(), before, "組み込みは不変");

    let bus = builtin(&app, MASTER_TRACK_ID, NativeKind::BusComp);
    native_edit(&mut app, bus, NativeEdit::param(NativeParamId::BusComp(common::model::BusCompParam::Threshold), -6.0));
    let touched = app.cur.peph.last_touched_param.clone().expect("last touched");
    assert_eq!(touched.track_id, MASTER_TRACK_ID);
    assert!(matches!(touched.target, AutomationTarget::NativeParam { device_id, .. } if device_id == bus));
}

/// ④ 単体 native の bypass は last touched を On にし、値 IPC を出さず LoadSong で届く。
#[test]
fn bypassing_one_native_device_touches_on_and_travels_by_load_song() {
    let (mut app, mut audio_rx, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    let eq = builtin(&app, track, NativeKind::Eq);
    native_edit(&mut app, eq, NativeEdit::param(NativeParamId::Eq { band: EqBand::Lmf, param: EqParam::Gain }, 4.0));
    app.flush_song_sync();
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![eq], bypassed: true }));
    let touched = app.cur.peph.last_touched_param.clone().expect("last touched");
    assert_eq!(touched.target, AutomationTarget::NativeParam { device_id: eq, param: NativeParamId::On(NativeKind::Eq) });
    assert_eq!(touched.track_id, track);
    app.flush_song_sync();
    let sent = drain(&mut audio_rx);
    assert!(!sent.iter().any(|c| matches!(c, AudioCommand::SetNativeDevice { .. })), "値 IPC は出さない");
    let loaded = sent
        .iter()
        .find_map(|c| match c {
            AudioCommand::LoadSong { song, .. } => Some(song),
            _ => None,
        })
        .expect("LoadSong が構造として運ぶ");
    assert!(loaded.native_by_id(eq).expect("eq").bypassed);
}

/// ⑤ SC Listen: Song の外。bypass 中なら有効化の 1 step だけ。消えた device は解除。新規は None。
#[test]
fn sc_listen_lives_outside_the_song() {
    let (mut app, mut audio_rx, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    let comp = builtin(&app, track, NativeKind::Comp);
    native_edit(&mut app, comp, NativeEdit::param(thr(), -12.0));
    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    let _ = drain(&mut audio_rx);

    // active 中: IPC だけ。
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: Some(comp) }));
    assert_eq!(app.cur.peph.sc_listen_device, Some(comp));
    assert_eq!(sc_listen_cmds(&mut audio_rx), vec![Some(comp)]);
    assert!(!app.cur.song_doc.is_dirty(), "Listen は dirty にしない");
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: None }));
    assert_eq!(sc_listen_cmds(&mut audio_rx), vec![None]);

    // bypass 中: 有効化で undo +1。
    app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![comp], bypassed: true }));
    let depth = app.cur.song_doc.undo_depth();
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: Some(comp) }));
    assert!(!app.cur.song_doc.song().native_by_id(comp).unwrap().bypassed);
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1);
    assert_eq!(app.cur.song_doc.history_labels().last().copied(), Some("デバイスを有効化"));

    // Comp 以外は受けない。
    let eq = builtin(&app, track, NativeKind::Eq);
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: Some(eq) }));
    assert_eq!(app.cur.peph.sc_listen_device, Some(comp));

    // Listen 中の device を削除 → 解除 + IPC。
    app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
        chain: ChainRef::Track(track),
        kind: NativeKind::Comp,
        open_panel: false,
    }));
    let added = app.cur.selection.selected_device_ids[0];
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: Some(added) }));
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![added] }));
    assert_eq!(app.cur.peph.sc_listen_device, None);
    assert_eq!(sc_listen_cmds(&mut audio_rx), vec![None]);

    // 新規は None。
    app.handle_event(AppEvent::Device(DeviceEvent::SetScListen { device_id: Some(comp) }));
    app.cur.song_doc.mark_saved();
    app.handle_event(AppEvent::New);
    assert_eq!(app.cur.peph.sc_listen_device, None);
}

/// ⑥ GR の表示: plane の値を正の減衰量で持ち、None の tick は前回値を保ち、減衰して消える。
#[test]
fn native_gain_reduction_display_follows_the_plane() {
    let (mut app, _a, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    let comp = builtin(&app, track, NativeKind::Comp);
    let peaks = vec![(0.0, 0.0); app.cur.song_doc.song().tracks.len()];
    let tick = |app: &mut AppData, native: Option<Vec<(u64, f32)>>| {
        let project = app.pk();
        app.handle_event(AppEvent::TrackPeaksTick {
            project,
            tracks: peaks.clone(),
            native_gr: native,
            master_limiter_gr_db: 0.0,
        });
    };
    let quiet = app.tick_visual_fingerprint();
    tick(&mut app, Some(vec![(comp, -6.0)]));
    assert!((app.cur.transport.native_gr.get(comp) - 6.0).abs() < 1e-4);
    assert_ne!(app.tick_visual_fingerprint(), quiet, "GR が動けば再描画する");
    tick(&mut app, None);
    assert!((app.cur.transport.native_gr.get(comp) - 6.0).abs() < 1e-4, "読めなかった tick は前回値");
    for _ in 0..200 {
        tick(&mut app, Some(Vec::new()));
    }
    assert_eq!(app.cur.transport.native_gr.get(comp), 0.0);
    assert_eq!(app.cur.transport.native_gr.iter().count(), 0, "0 に収束した id は捨てる");
}

/// ⑦ Mixer 帯のセクション開閉は見方の都合。
#[test]
fn toggling_strip_sections_does_not_dirty_the_song() {
    let (mut app, _a, _p, _d) = support::build_app();
    assert!(!app.cur.song_doc.is_dirty());
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    assert!(app.cur.view.strip_eq_open && app.cur.view.strip_comp_open);
    assert!(!app.cur.song_doc.is_dirty(), "見方の都合で `*` が立ってはいけない");
}

/// ⑧ 追加 EQ の削除でレーン / 変調 / その深さのレーンが消え、undo で全部戻る。
#[test]
fn removing_an_added_eq_prunes_its_bindings_and_undo_restores_them() {
    let (mut app, _a, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::Device(DeviceEvent::AddNative {
        chain: ChainRef::Track(track),
        kind: NativeKind::Eq,
        open_panel: false,
    }));
    let eq = app.cur.selection.selected_device_ids[0];
    let target = AutomationTarget::NativeParam { device_id: eq, param: NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain } };
    app.edit_song(|song| {
        song.mod_sources.push(ModSource {
            id: 1,
            owner_track_id: track,
            color: [1.0; 3],
            kind: ModSourceKind::default(),
            enabled: true,
        });
        let routing_id = song.alloc_mod_routing_id();
        song.push_lane(track, AutomationLane::new(target.clone(), 3.0));
        song.track_by_id_mut(track).unwrap().mod_routings.push(ModRouting {
            id: routing_id,
            target: target.clone(),
            source_id: 1,
            depth: 0.5,
            polarity: Polarity::Unipolar,
            enabled: true,
        });
        song.push_lane(track, AutomationLane::new(AutomationTarget::ModRoutingDepth { routing_id }, 0.5));
    });
    let count = |app: &AppData| {
        let t = app.cur.song_doc.song().track_by_id(track).unwrap();
        (t.automation_lanes.len(), t.mod_routings.len())
    };
    assert_eq!(count(&app), (2, 1));

    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![eq] }));
    assert_eq!(count(&app), (0, 0), "レーン / 変調 / 深さのレーンが連鎖して消える");
    app.handle_event(AppEvent::Undo);
    assert!(app.cur.song_doc.song().native_by_id(eq).is_some());
    assert_eq!(count(&app), (2, 1), "undo で全部戻る");
}

/// ⑨ Parallel の解除で Parallel / chain の住所を指すレーンが消える。
#[test]
fn ungrouping_a_parallel_prunes_its_lanes() {
    let (mut app, _a, _p, _d) = support::build_app();
    let track = app.cur.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::Device(DeviceEvent::AddParallel { chain: ChainRef::Track(track), at: InsertAt::Default }));
    let parallel = app.cur.song_doc.song().tracks[0].devices.iter().find_map(|d| d.as_parallel()).cloned().expect("Parallel");
    let (pid, cid) = (parallel.id, parallel.chains[0].id);
    app.edit_song(|song| {
        for target in [
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelOutGain { parallel_id: pid }),
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: cid }),
        ] {
            song.push_lane(track, AutomationLane::new(target, 1.0));
        }
    });
    assert_eq!(app.cur.song_doc.song().tracks[0].automation_lanes.len(), 2);
    app.handle_event(AppEvent::Device(DeviceEvent::UngroupParallel { parallel_id: pid }));
    assert!(app.cur.song_doc.song().tracks[0].automation_lanes.is_empty());
}

/// ⑩ トラック追加で組み込み 2 個 (bypass) が同じ undo step で入る。
#[test]
fn adding_a_track_supplies_builtins_in_the_same_undo_step() {
    let (mut app, _a, _p, _d) = support::build_app();
    let depth = app.cur.song_doc.undo_depth();
    let before = app.cur.song_doc.song().tracks.len();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let song = app.cur.song_doc.song();
    assert_eq!(song.tracks.len(), before + 1);
    let new_track = song.tracks.iter().find(|t| t.devices.len() == 2 && t.name.ends_with(&(before + 1).to_string())).expect("新トラック");
    let kinds: Vec<(NativeKind, bool)> =
        new_track.devices.iter().filter_map(|d| d.as_native()).map(|n| (n.kind(), n.bypassed && n.builtin)).collect();
    assert_eq!(kinds, vec![(NativeKind::Comp, true), (NativeKind::Eq, true)], "末尾に Comp → EQ (bypass)");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "補充は同じ step");
    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.song_doc.song().tracks.len(), before);
}
