//! トラックの親の付け替え (アレンジのヘッダ drop = `SetTrackParent`) が依存 (親子 / サイドチェイン /
//! send) の循環を作らない (`docs/plan_rack_native_devices.md` §5.9)。循環した graph は engine が空の
//! schedule にして master が無音になるので、編集の口で拒否して status を出す。

use common::model::{NativeKind, TapSource};
use common::routing_deps::{EdgeScope, TrackDeps};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_device::DeviceEvent;

use super::support::build_app;

fn add_track(app: &mut AppData) -> u32 {
    let before: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|id| !before.contains(id)).expect("新トラック")
}

/// ヘッダ drop と同じ event で `track` を `parent` の子にし、親の直後 (top-level なら先頭) へ置く。
fn reparent(app: &mut AppData, track: u32, parent: Option<u32>) {
    app.handle_event(AppEvent::SetTrackParent { track_ids: vec![track], parent_id: parent, anchor_after: parent });
}

fn parent(app: &AppData, id: u32) -> Option<u32> {
    app.cur.song_doc.song().track_by_id(id).expect("track").parent_group_id
}

fn acyclic(app: &AppData) -> bool {
    TrackDeps::build(app.cur.song_doc.song(), EdgeScope::Structural).dependency_order().is_ok()
}

/// G (子 D を持つ group) と top-level の C。
fn group_and_outsider(app: &mut AppData) -> (u32, u32, u32) {
    let g = app.cur.song_doc.song().tracks[0].id;
    let d = add_track(app);
    let c = add_track(app);
    reparent(app, d, Some(g));
    assert_eq!(parent(app, d), Some(g));
    (g, d, c)
}

/// C の組み込み Comp が G をサイドチェインに読んでいるとき、C を G の子にする付け替えは拒否される
/// (G←C の children 辺と C←G の SC 辺で循環する)。Song も undo も変わらず、status に理由が出る。
#[test]
fn reparent_into_a_group_that_the_track_sidechains_from_is_rejected() {
    let (mut app, _a, _p, _d) = build_app();
    let (g, _d, c) = group_and_outsider(&mut app);
    let comp = app.cur.song_doc.song().builtin_native(c, NativeKind::Comp).expect("comp").id;
    app.handle_event(AppEvent::Device(DeviceEvent::SetSidechainSource {
        device_id: comp,
        port: 0,
        source: Some(TapSource::Track(g)),
    }));
    assert!(app.cur.song_doc.song().native_by_id(comp).expect("comp").aux_input.is_some(), "G は C に依存しないので配線できる");

    let song = app.cur.song_doc.song().clone();
    let depth = app.cur.song_doc.undo_depth();
    reparent(&mut app, c, Some(g));
    assert_eq!(parent(&app, c), None, "付け替えは拒否");
    assert_eq!(*app.cur.song_doc.song(), song, "並べ替えも起きない");
    assert!(acyclic(&app), "依存は循環しない");
    assert_eq!(app.cur.song_doc.undo_depth(), depth, "undo は増えない");
    assert!(app.ui_ephemeral.status_message.contains("循環"), "{}", app.ui_ephemeral.status_message);
}

/// G → C の send を張ってから C を G に入れるのも同じく拒否 (send の辺も数える)。
#[test]
fn reparent_into_a_group_that_sends_to_the_track_is_rejected() {
    let (mut app, _a, _p, _d) = build_app();
    let (g, _d, c) = group_and_outsider(&mut app);
    app.handle_event(AppEvent::AddSend { src_track_id: g, dest_track_id: c });
    assert_eq!(app.cur.song_doc.song().track_by_id(g).expect("g").sends.len(), 1, "G→C の send は循環しない");

    reparent(&mut app, c, Some(g));
    assert_eq!(parent(&app, c), None, "付け替えは拒否");
    assert!(acyclic(&app));
}

/// group を自分の子の中へ入れる付け替えも拒否 (親子の鎖の循環も同じ 1 つの判定)。循環しない付け替えは通り、
/// drop の並び (`anchor_after` の直後) も同じ口で決まる。
#[test]
fn reparent_into_own_descendant_is_rejected_and_legal_moves_pass() {
    let (mut app, _a, _p, _d) = build_app();
    let (g, d, c) = group_and_outsider(&mut app);
    reparent(&mut app, g, Some(d));
    assert_eq!(parent(&app, g), None, "自分の子の中へは入れない");
    assert!(acyclic(&app));

    reparent(&mut app, c, Some(g));
    assert_eq!(parent(&app, c), Some(g), "循環しない付け替えは通る");
    let order: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    assert_eq!(order.iter().position(|&id| id == c), order.iter().position(|&id| id == g).map(|i| i + 1), "親の直後");
    reparent(&mut app, c, None);
    assert_eq!(parent(&app, c), None, "top-level へ戻せる");
    assert_eq!(app.cur.song_doc.song().tracks[0].id, c, "anchor なしは先頭");
}
