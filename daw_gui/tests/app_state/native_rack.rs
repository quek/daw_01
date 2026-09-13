//! r.md #129 (`docs/plan_rack_native_devices.md` §15.7 R-1〜R-9): Rack の内蔵 device の headless 回帰 —
//! 挿入位置 / 組み込みのガード / コピーと番号 / 束縛の運搬 / トラック複製 / Par の開閉と保存 /
//! サイドチェインの配線と循環の除外 / picker / スペクトラム要求。
//!
//! `Q` の宛先 (R-10) は view の dispatch を通すので `tests/native_rack_visual.rs`。

use std::sync::Arc;

use common::model::{
    AutomationLane, AutomationTarget, ChainRef, CompParam, Device, MASTER_TRACK_ID, ModRouting, ModSource,
    ModSourceKind, NativeDevice, NativeKind, NativeParamId, Polarity, RackPanelKey, TapPoint, TapSource,
    TrackBuiltinParam,
};
use common::plugin_db::{NATIVE_COMP_PICKER_ID, NATIVE_EQ_PICKER_ID, PARALLEL_PICKER_ID};
use common::protocol::{AudioCommand, PluginEvent};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, InsertAt, ParamSurface, RelocateDevices};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::event_tabs::TabEvent;

use super::support::{build_app, drain, fake_plugin_loaded};

// ---- 共通 ----------------------------------------------------------------------------------

fn dev(app: &mut AppData, ev: DeviceEvent) {
    app.handle_event(AppEvent::Device(ev));
}

fn track_id(app: &AppData, idx: usize) -> u32 {
    app.cur.song_doc.song().tracks[idx].id
}

fn builtin(app: &AppData, owner: u32, kind: NativeKind) -> u64 {
    app.cur.song_doc.song().builtin_native(owner, kind).expect("組み込みがある").id
}

fn native(app: &AppData, id: u64) -> NativeDevice {
    *app.cur.song_doc.song().native_by_id(id).expect("内蔵 device")
}

/// `owner` の最上位の並び: plugin は plugin_id、内蔵は表示名 (組み込みは `*`)、Parallel は `P`。
fn top(app: &AppData, owner: u32) -> Vec<String> {
    app.cur
        .song_doc
        .song()
        .fx_chain_by_track_id(owner)
        .expect("chain")
        .iter()
        .map(|d| match d {
            Device::Plugin(p) => p.plugin_id.clone(),
            Device::Native(n) => format!("{}{}", n.display_name(), if n.builtin { "*" } else { "" }),
            Device::Parallel(_) => "P".to_string(),
        })
        .collect()
}

/// cursor を `owner` にして picker で `id` を選ぶ (Shift なし = GUI / Par を開く)。
fn pick(app: &mut AppData, owner: u32, id: &str) {
    select(app, owner);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb { id: id.into(), keep_open: false, open_gui: true });
}

fn select(app: &mut AppData, owner: u32) {
    if owner == MASTER_TRACK_ID {
        app.cur.selection.selected_track_ids = vec![MASTER_TRACK_ID];
    } else {
        let visible: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
        app.apply_select_tracks(owner, daw_gui::widgets::select_modifier::SelectModifier::Single, &visible);
    }
}

fn add_track(app: &mut AppData) -> u32 {
    let before: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|id| !before.contains(id)).expect("新トラック")
}

fn flush_states(app: &mut AppData) {
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
}

fn relocate(app: &mut AppData, device_ids: Vec<u64>, dest: ChainRef, dest_index: InsertAt, copy: bool) {
    dev(app, DeviceEvent::RelocateDevices(RelocateDevices { device_ids, dest, dest_index, copy }));
}

/// `owner` の最上位の `index` 番目の device id。
fn top_id(app: &AppData, owner: u32, index: usize) -> u64 {
    app.cur.song_doc.song().fx_chain_by_track_id(owner).expect("chain")[index].id()
}

/// plugin を picker で足して load 完了まで進める。戻り値 = device id。
fn load_plugin(app: &mut AppData, owner: u32, plugin_id: &str) -> u64 {
    pick(app, owner, plugin_id);
    let index = app.cur.song_doc.song().fx_chain_by_track_id(owner).map_or(0, |c| common::model::plugins(c).count()) - 1;
    fake_plugin_loaded(app, owner, index as u32, plugin_id)
}

// ---- R-1 挿入位置 ---------------------------------------------------------------------------

/// R-1: 新しいデバイスは「組み込み以外で一番下にあるデバイスの直後」。組み込み以外が無ければ通常トラックは
/// 組み込みの上、master は下。picker の Parallel / 内蔵 / `RelocateDevices{Default}` が同じ規則で、deferred の
/// 運搬は **実行時** の Song で解決する。
#[test]
fn new_devices_go_right_after_the_last_non_builtin_device() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    // [Parallel A, Comp*, EQ*] に内蔵 Comp → A の直後に「Comp 2」。
    pick(&mut app, t0, PARALLEL_PICKER_ID);
    assert_eq!(top(&app, t0), ["P", "Comp*", "EQ*"], "組み込み以外 0 個の通常トラックは組み込みの上");
    pick(&mut app, t0, NATIVE_COMP_PICKER_ID);
    assert_eq!(top(&app, t0), ["P", "Comp 2", "Comp*", "EQ*"]);

    // [Comp*, EQ*, A] に足すと A の直後 (= 末尾)。
    let t1 = add_track(&mut app);
    pick(&mut app, t1, PARALLEL_PICKER_ID);
    let a = top_id(&app, t1, 0);
    relocate(&mut app, vec![a], ChainRef::Track(t1), InsertAt::Index(3), false);
    assert_eq!(top(&app, t1), ["Comp*", "EQ*", "P"]);
    pick(&mut app, t1, NATIVE_COMP_PICKER_ID);
    assert_eq!(top(&app, t1), ["Comp*", "EQ*", "P", "Comp 2"]);

    // master は組み込みの下。
    pick(&mut app, MASTER_TRACK_ID, NATIVE_EQ_PICKER_ID);
    assert_eq!(top(&app, MASTER_TRACK_ID), ["Bus Comp*", "Tone EQ*", "EQ"]);

    // `RelocateDevices{Default}` も同じ位置 (組み込み以外 0 個 → 組み込みの上)。
    let t2 = add_track(&mut app);
    relocate(&mut app, vec![a], ChainRef::Track(t2), InsertAt::Default, false);
    assert_eq!(top(&app, t2), ["P", "Comp*", "EQ*"]);

    // deferred: plugin が居ると運搬は round-trip 待ちに積まれる。その間に内蔵を足しても、Default は
    // 実行時の Song で解決されるので「足した EQ 2 の直後」に入る (積んだ時点の index なら楽器の直後)。
    let t3 = add_track(&mut app);
    let synth = load_plugin(&mut app, t3, "test.synth");
    assert_eq!(top(&app, t3), ["test.synth", "Comp*", "EQ*"]);
    relocate(&mut app, vec![a], ChainRef::Track(t3), InsertAt::Default, false);
    assert_eq!(top(&app, t3), ["test.synth", "Comp*", "EQ*"], "まだ round-trip 待ち");
    pick(&mut app, t3, NATIVE_EQ_PICKER_ID);
    assert_eq!(top(&app, t3), ["test.synth", "EQ 2", "Comp*", "EQ*"], "楽器 (組み込み以外) の直後");
    flush_states(&mut app);
    assert_eq!(top(&app, t3), ["test.synth", "EQ 2", "P", "Comp*", "EQ*"]);
    assert_eq!(top_id(&app, t3, 0), synth);
}

// ---- R-2 ガード ----------------------------------------------------------------------------

/// R-2 (G1): 組み込みを含む選択に削除 / 切り取り / Parallel 化 / 他トラックへの移動 / Parallel の中への移動を
/// 掛けても、組み込みは id・値・bypass・レーンごと残る (正規化の補充で「元に戻った」ように見えるのではない)。
/// 組み込みだけの削除は Song も undo も round-trip も動かさず、status で理由を出す。
#[test]
fn builtin_natives_survive_every_removal_and_carry_path() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    let t1 = add_track(&mut app);
    let synth = load_plugin(&mut app, t0, "test.synth");
    let comp = builtin(&app, t0, NativeKind::Comp);
    let eq = builtin(&app, t0, NativeKind::Eq);
    dev(&mut app, DeviceEvent::NativeEdit {
        device_id: comp,
        edit: daw_gui::event_native::NativeEdit::param(NativeParamId::Comp(CompParam::Threshold), -24.0),
    });
    let thr = AutomationTarget::NativeParam { device_id: comp, param: NativeParamId::Comp(CompParam::Threshold) };
    app.edit_song(|s| s.push_lane(t0, AutomationLane::new(thr.clone(), 0.5)));
    let before_comp = native(&app, comp);
    let before_eq = native(&app, eq);
    let survived = |app: &AppData| {
        assert_eq!(native(app, comp), before_comp, "組み込み Comp が id・値・bypass ごと残る");
        assert_eq!(native(app, eq), before_eq);
        assert!(app.cur.song_doc.song().track_by_id(t0).unwrap().automation_lanes.iter().any(|l| l.target == thr), "レーンも残る");
        assert!(!app.cur.song_doc.song().clone().normalize_native_devices(), "補充は起きていない");
    };

    // 組み込みだけの削除: 何も積まない。
    let song = app.cur.song_doc.song().clone();
    let depth = app.cur.song_doc.undo_depth();
    let queued = app.cur.pipc.pending_state_queue.len();
    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![comp, eq] });
    assert_eq!(*app.cur.song_doc.song(), song);
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert_eq!(app.cur.pipc.pending_state_queue.len(), queued, "round-trip も積まない");
    assert!(app.ui_ephemeral.status_message.contains("削除できません"), "{}", app.ui_ephemeral.status_message);
    survived(&app);

    // Parallel 化: plugin だけが包まれる。
    dev(&mut app, DeviceEvent::GroupDevices { device_ids: vec![synth, comp] });
    assert_eq!(top(&app, t0), ["P", "Comp*", "EQ*"]);
    assert!(app.ui_ephemeral.status_message.contains("Parallel"), "{}", app.ui_ephemeral.status_message);
    survived(&app);
    let parallel = top_id(&app, t0, 0);
    let chain = app.cur.song_doc.song().parallel_by_id(parallel).unwrap().chains[0].id;

    // Parallel の中へ普通のドラッグ: 動かない。
    relocate(&mut app, vec![eq], ChainRef::Chain(chain), InsertAt::Index(0), false);
    flush_states(&mut app);
    survived(&app);
    // 他トラックへ普通のドラッグ: 動かない (status)。
    relocate(&mut app, vec![comp], ChainRef::Track(t1), InsertAt::Default, false);
    flush_states(&mut app);
    assert!(app.ui_ephemeral.status_message.contains("移動できません"), "{}", app.ui_ephemeral.status_message);
    assert_eq!(top(&app, t1), ["Comp*", "EQ*"]);
    survived(&app);

    // 切り取り: 組み込みはクリップボードに載らず消えない。Parallel (plugin 入り) だけが切り取られる。
    app.ui_ephemeral.pending_clipboard_write = None;
    app.cur.selection.selected_device_ids = vec![parallel, comp, eq];
    app.cut_devices(vec![parallel, comp, eq]);
    flush_states(&mut app);
    let json = app.ui_ephemeral.pending_clipboard_write.clone().expect("Parallel はクリップボードに載る");
    let env = daw_gui::clipboard::ClipboardEnvelope::from_json(&json).expect("envelope");
    let daw_gui::clipboard::ClipboardPayload::Devices(copied) = env.payload else { panic!("devices") };
    assert_eq!(copied.iter().map(|d| d.device.id()).collect::<Vec<_>>(), vec![parallel]);
    assert_eq!(top(&app, t0), ["Comp*", "EQ*"]);
    survived(&app);

    // 選択に組み込みが混ざった削除: 組み込みだけが残る。
    let delay = load_plugin(&mut app, t0, "test.delay");
    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![delay, comp, eq] });
    flush_states(&mut app);
    assert_eq!(top(&app, t0), ["Comp*", "EQ*"]);
    survived(&app);

    // master の組み込みも同じ。
    let bus = builtin(&app, MASTER_TRACK_ID, NativeKind::BusComp);
    let before_bus = native(&app, bus);
    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![bus] });
    dev(&mut app, DeviceEvent::GroupDevices { device_ids: vec![bus] });
    flush_states(&mut app);
    assert_eq!(native(&app, bus), before_bus);
    assert_eq!(top(&app, MASTER_TRACK_ID), ["Bus Comp*", "Tone EQ*"]);
}

// ---- R-3 コピー ----------------------------------------------------------------------------

/// R-3 (G2): 組み込みを Ctrl でコピーすると「追加の Comp」になる (同じトラックでも他トラックでも)。番号は空き番号、
/// 値は同じ、レーンは複製しない。
#[test]
fn copying_a_builtin_makes_an_added_device_with_a_fresh_ordinal() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    let t1 = add_track(&mut app);
    let comp = builtin(&app, t0, NativeKind::Comp);
    dev(&mut app, DeviceEvent::NativeEdit {
        device_id: comp,
        edit: daw_gui::event_native::NativeEdit::param(NativeParamId::Comp(CompParam::Ratio), 8.0),
    });
    app.edit_song(|s| {
        s.push_lane(t0, AutomationLane::new(AutomationTarget::NativeParam { device_id: comp, param: NativeParamId::Comp(CompParam::Ratio) }, 0.5))
    });
    let lanes_before = app.cur.song_doc.song().track_by_id(t0).unwrap().automation_lanes.len();

    relocate(&mut app, vec![comp], ChainRef::Track(t0), InsertAt::Index(0), true);
    assert_eq!(top(&app, t0), ["Comp 2", "Comp*", "EQ*"]);
    let copy = top_id(&app, t0, 0);
    assert_ne!(copy, comp);
    let (c, orig) = (native(&app, copy), native(&app, comp));
    assert!(!c.builtin && c.ordinal == 2);
    assert_eq!(c.params, orig.params, "値は同じ");
    assert_eq!(app.cur.song_doc.song().track_by_id(t0).unwrap().automation_lanes.len(), lanes_before, "レーンは複製しない");

    // 他トラックへのコピーも追加分 (そこにも組み込み Comp があるので 2)。2 個目は 3。
    relocate(&mut app, vec![comp], ChainRef::Track(t1), InsertAt::Default, true);
    relocate(&mut app, vec![comp], ChainRef::Track(t1), InsertAt::Default, true);
    assert_eq!(top(&app, t1), ["Comp 2", "Comp 3", "Comp*", "EQ*"]);
}

// ---- R-4 束縛の運搬 ------------------------------------------------------------------------

/// R-4 (T16): Parallel の中の追加 Comp を他トラックへ運ぶと、そのレーン / 変調 / 深さのレーン / 同じ Parallel の
/// ChainGain への変調とその深さが移動先へ再採番されて移り、ジェスチャーの鍵も面を保ったまま付け替わる。
/// master で足した「Comp」を組み込み Comp のある通常トラックへ移すと「Comp 2」になる。
#[test]
fn carrying_a_parallel_moves_every_binding_under_it() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    let t1 = add_track(&mut app);
    pick(&mut app, t0, PARALLEL_PICKER_ID);
    let parallel = top_id(&app, t0, 0);
    let chain = app.cur.song_doc.song().parallel_by_id(parallel).unwrap().chains[0].id;
    dev(&mut app, DeviceEvent::AddNative { chain: ChainRef::Chain(chain), kind: NativeKind::Comp, open_panel: true });
    let comp2 = *app.cur.selection.selected_device_ids.last().unwrap();
    assert_eq!(native(&app, comp2).ordinal, 2);
    let thr = AutomationTarget::NativeParam { device_id: comp2, param: NativeParamId::Comp(CompParam::Threshold) };
    let ratio = AutomationTarget::NativeParam { device_id: comp2, param: NativeParamId::Comp(CompParam::Ratio) };
    let chain_gain = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: chain });
    let (r_ratio, r_gain) = app
        .edit_song(|s| {
            s.mod_sources.push(ModSource { id: 1, owner_track_id: t0, color: [1.0; 3], kind: ModSourceKind::default(), enabled: true });
            s.push_lane(t0, AutomationLane::new(thr.clone(), 0.5));
            let routing = |s: &mut common::model::Song, target: AutomationTarget| {
                let id = s.alloc_mod_routing_id();
                s.track_by_id_mut(t0).unwrap().mod_routings.push(ModRouting {
                    id,
                    target,
                    source_id: 1,
                    depth: 0.5,
                    polarity: Polarity::Unipolar,
                    enabled: true,
                });
                s.push_lane(t0, AutomationLane::new(AutomationTarget::ModRoutingDepth { routing_id: id }, 0.5));
                id
            };
            (routing(s, ratio.clone()), routing(s, chain_gain.clone()))
        })
        .unwrap();
    app.cur.recording.active_param_gestures.insert((t0, thr.clone()), ParamSurface::Rack);

    relocate(&mut app, vec![parallel], ChainRef::Track(t1), InsertAt::Default, false);
    let song = app.cur.song_doc.song();
    let (src, dst) = (song.track_by_id(t0).unwrap(), song.track_by_id(t1).unwrap());
    assert!(src.automation_lanes.is_empty() && src.mod_routings.is_empty(), "元トラックに何も残らない");
    let lane_targets: Vec<&AutomationTarget> = dst.automation_lanes.iter().map(|l| &l.target).collect();
    assert!(lane_targets.contains(&&thr));
    for id in [r_ratio, r_gain] {
        assert!(lane_targets.contains(&&AutomationTarget::ModRoutingDepth { routing_id: id }), "深さのレーン {id}");
    }
    let routing_targets: Vec<&AutomationTarget> = dst.mod_routings.iter().map(|r| &r.target).collect();
    assert_eq!(routing_targets, vec![&ratio, &chain_gain]);
    let mut lane_ids: Vec<u32> = dst.automation_lanes.iter().map(|l| l.id).collect();
    lane_ids.sort_unstable();
    lane_ids.dedup();
    assert_eq!(lane_ids.len(), dst.automation_lanes.len(), "lane id は移動先で再採番されて衝突しない");
    assert_eq!(app.cur.recording.active_param_gestures.get(&(t1, thr.clone())), Some(&ParamSurface::Rack), "面を保って付け替わる");
    assert!(!app.cur.recording.active_param_gestures.contains_key(&(t0, thr)));
    assert!(!app.rack_panel_open(RackPanelKey::Device(comp2)), "運んだ行の Par は閉じる (Q18)");

    // master で足した「Comp」(番号なし) を組み込み Comp のある通常トラックへ → 2。
    pick(&mut app, MASTER_TRACK_ID, NATIVE_COMP_PICKER_ID);
    let mcomp = *app.cur.selection.selected_device_ids.last().unwrap();
    assert_eq!(native(&app, mcomp).display_name(), "Comp");
    let t2 = add_track(&mut app);
    relocate(&mut app, vec![mcomp], ChainRef::Track(t2), InsertAt::Default, false);
    assert_eq!(native(&app, mcomp).display_name(), "Comp 2");
}

// ---- R-5 トラックの複製 ----------------------------------------------------------------------

/// R-5 (T22): トラックを複製すると、複製側のレーンは複製側の内蔵 device の id を指し、組み込みは組み込みのまま。
#[test]
fn duplicated_track_lanes_point_at_its_own_natives() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    let comp = builtin(&app, t0, NativeKind::Comp);
    let thr = AutomationTarget::NativeParam { device_id: comp, param: NativeParamId::Comp(CompParam::Threshold) };
    app.edit_song(|s| s.push_lane(t0, AutomationLane::new(thr, 0.5)));
    let before: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.handle_event(AppEvent::DuplicateTracksUnique(vec![t0]));
    let song = app.cur.song_doc.song();
    let dup = song.tracks.iter().find(|t| !before.contains(&t.id)).expect("複製トラック");
    let dup_comp = song.builtin_native(dup.id, NativeKind::Comp).expect("複製側にも組み込み Comp");
    assert_ne!(dup_comp.id, comp);
    assert!(dup_comp.builtin);
    assert_eq!(
        dup.automation_lanes.iter().map(|l| l.target.clone()).collect::<Vec<_>>(),
        vec![AutomationTarget::NativeParam { device_id: dup_comp.id, param: NativeParamId::Comp(CompParam::Threshold) }]
    );
}

// ---- R-6 Par の開閉と保存 --------------------------------------------------------------------

/// R-6 (Q11 / Q18): 内蔵 Comp と EQ の Par は同時に開ける。開いている集合は保存されて別の app で戻り、`*` は立たない。
/// 移動で閉じ、コピー先は閉じた状態、削除で外れ、表示状態の無い旧ファイルでは空になる。
#[test]
fn rack_panels_open_independently_and_persist_without_dirtying() {
    let (mut app, _a, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    select(&mut app, t0);
    let (comp, eq) = (builtin(&app, t0, NativeKind::Comp), builtin(&app, t0, NativeKind::Eq));
    let dirty_before = app.cur.song_doc.is_dirty();
    dev(&mut app, DeviceEvent::ToggleRackPanel(RackPanelKey::Device(comp)));
    dev(&mut app, DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq)));
    dev(&mut app, DeviceEvent::ToggleRackPanel(RackPanelKey::MasterLimiter));
    assert!(app.rack_panel_open(RackPanelKey::Device(comp)) && app.rack_panel_open(RackPanelKey::Device(eq)));
    assert_eq!(app.cur.song_doc.is_dirty(), dirty_before, "開閉は `*` を立てない");

    let snap = app.snapshot_view_state();
    assert!(comp < eq, "組み込みは Comp → EQ の順に採番される");
    assert_eq!(snap.open_rack_panels, vec![RackPanelKey::Device(comp), RackPanelKey::Device(eq), RackPanelKey::MasterLimiter], "キー順で保存");
    let (mut other, _a2, _p2, _d2) = build_app();
    other.cur.song_doc.replace_song(app.cur.song_doc.song().clone());
    other.restore_view_state(Some(snap), common::model::LoopRegion::default(), Vec::new());
    assert_eq!(other.cur.view.open_rack_panels, app.cur.view.open_rack_panels);
    assert!(!other.cur.song_doc.is_dirty());

    // 移動 (同じトラック内の並べ替え) で閉じる。コピー先は閉じている。
    relocate(&mut app, vec![eq], ChainRef::Track(t0), InsertAt::Index(0), false);
    assert!(!app.rack_panel_open(RackPanelKey::Device(eq)), "並べ替えでも閉じる");
    relocate(&mut app, vec![comp], ChainRef::Track(t0), InsertAt::Index(0), true);
    let copy = top_id(&app, t0, 0);
    assert!(app.rack_panel_open(RackPanelKey::Device(comp)) && !app.rack_panel_open(RackPanelKey::Device(copy)));
    // 削除で外れる。
    dev(&mut app, DeviceEvent::ToggleRackPanel(RackPanelKey::Device(copy)));
    dev(&mut app, DeviceEvent::RemoveDevices { device_ids: vec![copy] });
    assert!(!app.rack_panel_open(RackPanelKey::Device(copy)));
    // 表示状態の無い旧ファイル: 前の集合を持ち越さない。
    app.restore_view_state(None, common::model::LoopRegion::default(), Vec::new());
    assert!(app.cur.view.open_rack_panels.is_empty());
}

// ---- R-7 サイドチェイン ----------------------------------------------------------------------

/// R-7 (Q19 / §10.13): 内蔵 Comp は plugin と同じ手順で外部サイドチェインを配線でき、EQ は受けない。
/// 子トラックの Comp の候補に親 group は出ず、直接指定しても拒否されて undo は増えない (send の候補も同じ)。
#[test]
fn native_comp_sidechain_wiring_excludes_cyclic_sources() {
    let (mut app, _a, _p, _d) = build_app();
    let g = track_id(&app, 0);
    let a = add_track(&mut app);
    let b = add_track(&mut app);
    app.edit_song(|s| s.track_by_id_mut(a).unwrap().parent_group_id = Some(g));
    let comp_a = builtin(&app, a, NativeKind::Comp);
    let eq_a = builtin(&app, a, NativeKind::Eq);

    dev(&mut app, DeviceEvent::SetSidechainSource { device_id: comp_a, port: 0, source: Some(TapSource::Track(b)) });
    assert_eq!(native(&app, comp_a).aux_input.map(|r| r.tap.source), Some(TapSource::Track(b)));
    dev(&mut app, DeviceEvent::SetAuxInputTapPoint { device_id: comp_a, port: 0, tap_point: TapPoint::PostFx });
    assert_eq!(native(&app, comp_a).aux_input.map(|r| r.tap.tap_point), Some(TapPoint::PostFx));
    assert_eq!(app.sidechain_ports(comp_a).len(), 1, "内蔵 Comp は port 0 の 1 本");
    dev(&mut app, DeviceEvent::SetSidechainSource { device_id: eq_a, port: 0, source: Some(TapSource::Track(b)) });
    assert_eq!(native(&app, eq_a).aux_input, None, "EQ は受けない");
    assert!(app.sidechain_ports(eq_a).is_empty());

    select(&mut app, a);
    let choices = app.sidechain_source_choices(comp_a);
    assert!(choices.iter().any(|c| c.source == Some(TapSource::Track(b))));
    assert!(!choices.iter().any(|c| c.source == Some(TapSource::Track(g))), "子の Comp の候補に親 group は出ない");
    let depth = app.cur.song_doc.undo_depth();
    dev(&mut app, DeviceEvent::SetSidechainSource { device_id: comp_a, port: 0, source: Some(TapSource::Track(g)) });
    assert_eq!(native(&app, comp_a).aux_input.map(|r| r.tap.source), Some(TapSource::Track(b)), "直接指定も拒否");
    assert_eq!(app.cur.song_doc.undo_depth(), depth, "undo は増えない");

    // send: 親 group から子への send は循環するので候補に出ない。
    assert!(!app.send_destination_candidates(g).iter().any(|(id, _)| *id == a));
    assert!(app.send_destination_candidates(b).iter().any(|(id, _)| *id == a));
}

// ---- R-8 picker ----------------------------------------------------------------------------

fn build_app_without_db() -> (AppData, UnboundedReceiver<AudioCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    (app, audio_rx)
}

/// R-8 (Q8): 内蔵 4 種は DB が無くても picker の先頭に固定順で出て、master でも見え、検索 / 種別 `f ` で絞れ、
/// Ctrl (keep_open) で開いたまま足せる (Shift なしなら Par が開く)。
#[test]
fn picker_lists_native_devices_first_and_filters_them() {
    let (mut app, _audio) = build_app_without_db();
    let names: Vec<String> = app.ui_ephemeral.plugin_picker_entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names[..4], ["Comp", "EQ", "Bus Comp", "Tone EQ"], "DB が無くても内蔵 4 種が先頭に固定順");
    assert!(names.iter().any(|n| n.starts_with("Parallel")), "Parallel も出る: {names:?}");

    select(&mut app, MASTER_TRACK_ID);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    let visible = |app: &AppData| app.ui_ephemeral.plugin_picker_visible.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    assert_eq!(visible(&app)[..4], ["Comp", "EQ", "Bus Comp", "Tone EQ"], "master でも見える");
    for query in ["comp", "f comp"] {
        app.handle_event(AppEvent::SetPluginPickerQuery(query.into()));
        assert_eq!(visible(&app), ["Comp", "Bus Comp"], "{query}");
    }
    app.handle_event(AppEvent::SetPluginPickerQuery("i ".into()));
    assert!(visible(&app).is_empty(), "楽器の種別には出ない");

    let (mut app, _audio2, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    select(&mut app, t0);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb { id: NATIVE_EQ_PICKER_ID.into(), keep_open: true, open_gui: true });
    app.handle_event(AppEvent::SelectPluginFromDb { id: NATIVE_EQ_PICKER_ID.into(), keep_open: true, open_gui: false });
    assert!(app.ui_ephemeral.is_plugin_picker_open, "Ctrl は開いたまま");
    assert_eq!(top(&app, t0), ["EQ 2", "EQ 3", "Comp*", "EQ*"]);
    assert!(app.rack_panel_open(RackPanelKey::Device(top_id(&app, t0, 0))), "Shift なしは Par を開く");
    assert!(!app.rack_panel_open(RackPanelKey::Device(top_id(&app, t0, 1))), "Shift は Par を開かない");
}

// ---- R-9 スペクトラム要求 --------------------------------------------------------------------

fn scope_msgs(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<Vec<u64>> {
    drain(rx)
        .into_iter()
        .filter_map(|c| match c {
            AudioCommand::SetDeviceScopes { device_ids, .. } => Some(device_ids),
            _ => None,
        })
        .collect()
}

/// R-9 (Q14 / §11.2): EQ の Par を開くと差分があったときだけ `SetDeviceScopes` を 1 通送る。Parallel を折り畳むと
/// 外れる。タブを切り替えて戻っても再送しない (送った集合はタブごとに覚えている)。
#[test]
fn device_scopes_are_sent_only_when_the_open_eq_panels_change() {
    let (mut app, mut audio, _p, _d) = build_app();
    let t0 = track_id(&app, 0);
    select(&mut app, t0);
    pick(&mut app, t0, PARALLEL_PICKER_ID);
    let parallel = top_id(&app, t0, 0);
    let chain = app.cur.song_doc.song().parallel_by_id(parallel).unwrap().chains[0].id;
    dev(&mut app, DeviceEvent::AddNative { chain: ChainRef::Chain(chain), kind: NativeKind::Eq, open_panel: true });
    let eq = *app.cur.selection.selected_device_ids.last().unwrap();
    let _ = drain(&mut audio);

    app.sync_device_scopes();
    assert_eq!(scope_msgs(&mut audio), vec![vec![eq]]);
    app.sync_device_scopes();
    assert!(scope_msgs(&mut audio).is_empty(), "変化が無ければ送らない");
    dev(&mut app, DeviceEvent::ToggleParallelNodeCollapsed { id: parallel });
    app.sync_device_scopes();
    assert_eq!(scope_msgs(&mut audio), vec![Vec::<u64>::new()], "折り畳んだ Parallel の中は外れる");
    dev(&mut app, DeviceEvent::ToggleParallelNodeCollapsed { id: parallel });
    app.sync_device_scopes();
    assert_eq!(scope_msgs(&mut audio), vec![vec![eq]]);

    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    app.sync_device_scopes();
    app.handle_event(AppEvent::Tab(TabEvent::Switch(a)));
    let _ = scope_msgs(&mut audio);
    app.sync_device_scopes();
    assert!(scope_msgs(&mut audio).is_empty(), "戻ったタブは覚えている集合と同じなので再送しない");
}
