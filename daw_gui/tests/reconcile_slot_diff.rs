//! `compute_slot_reconcile_actions` の device-level diff を unit test で
//! 固定する。
//!
//! r.md #71 (プラグインのコピー / 移動): アドレスは安定 `device_id` 一本
//! (track / chain 内 index は出てこない)。3 ケース (host extra / song extra /
//! plugin_id_str mismatch) + 完全一致 + state 伝搬を pure function 経由で固定する。

use std::collections::HashMap;

use common::model::{PluginInstance, Song};
use common::plugin_format::PluginFormat;

use daw_gui::app::{LoadedDeviceInfo, SlotReconcileAction, compute_slot_reconcile_actions};

/// 安定 id を焼き込んだ device を作る (`PluginInstance::new` は id == 0 の
/// 未採番 sentinel を返すので、 テストは明示的に振る)。
fn make_instance(device_id: u64, plugin_id: &str) -> PluginInstance {
    PluginInstance {
        id: device_id,
        ..PluginInstance::new(plugin_id.into(), PluginFormat::Clap)
    }
}

/// 1 track に flat な device 列を載せた Song を作る (役割は位置導出なので保持
/// しない)。
fn make_song_with_one_track(track_id: u32, devices: Vec<PluginInstance>) -> Song {
    let mut song = Song::default();
    song.tracks.push(daw_gui::app::track_with(|t| {
        t.id = track_id;
        t.name = "T".into();
        t.devices = devices;
    }));
    song
}

fn loaded(plugin_id_str: &str) -> LoadedDeviceInfo {
    LoadedDeviceInfo {
        plugin_id_str: plugin_id_str.into(),
    }
}

#[test]
fn host_extra_device_yields_remove_action() {
    // host に device 100 + 101 が居る、 song は 100 のみ → 101 を消す。
    let song = make_song_with_one_track(10, vec![make_instance(100, "p.comp")]);
    let mut loaded_devices = HashMap::new();
    loaded_devices.insert(100, loaded("p.comp"));
    loaded_devices.insert(101, loaded("p.reverb"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::RemoveDevice { device_id: 101 }],
        "host extra device 101 should be removed: {actions:?}"
    );
}

#[test]
fn song_extra_device_yields_load_action() {
    // host に 200 のみ、 song は 200 + 201 → 201 を load。
    let song = make_song_with_one_track(
        11,
        vec![make_instance(200, "p.comp"), make_instance(201, "p.reverb")],
    );
    let mut loaded_devices = HashMap::new();
    loaded_devices.insert(200, loaded("p.comp"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadDevice {
            device_id: 201,
            plugin_id_str: "p.reverb".into(),
            initial_state: None,
        }],
        "song extra device 201 should be loaded: {actions:?}"
    );
}

#[test]
fn plugin_id_mismatch_yields_load_action() {
    // host の device 300 = PluginA、 song の同 device = PluginB → Load (= 入れ替え)。
    // plugin_host 側の SetSlotPlugin handler が dedup logic で
    // 「同 device に違う plugin」 を見て unload + load を組み立てる前提。
    let song = make_song_with_one_track(12, vec![make_instance(300, "p.B")]);
    let mut loaded_devices = HashMap::new();
    loaded_devices.insert(300, loaded("p.A"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadDevice {
            device_id: 300,
            plugin_id_str: "p.B".into(),
            initial_state: None,
        }],
        "plugin_id_str mismatch should trigger LoadDevice: {actions:?}"
    );
}

#[test]
fn matching_devices_produce_no_action() {
    // host と song が完全一致 → action 空。 **チェーン順と host 側の登録順が
    // 食い違っていても** 一致とみなす (host は順序を持たない)。
    let song = make_song_with_one_track(
        13,
        vec![
            make_instance(402, "p.midi"),
            make_instance(400, "p.synth"),
            make_instance(401, "p.comp"),
        ],
    );
    let mut loaded_devices = HashMap::new();
    loaded_devices.insert(400, loaded("p.synth"));
    loaded_devices.insert(401, loaded("p.comp"));
    loaded_devices.insert(402, loaded("p.midi"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert!(
        actions.is_empty(),
        "perfectly synced track yields no actions: {actions:?}"
    );
}

#[test]
fn initial_state_propagates_to_load_action() {
    // Song.PluginInstance::state が LoadDevice::initial_state に伝搬する
    // ことを確認 (= Undo で knob 値復元の根幹)。
    let mut inst = make_instance(500, "p.synth");
    inst.state = Some(vec![1, 2, 3, 4].into());
    let song = make_song_with_one_track(14, vec![inst]);
    let loaded_devices = HashMap::new();

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadDevice {
            device_id: 500,
            plugin_id_str: "p.synth".into(),
            initial_state: Some(vec![1, 2, 3, 4]),
        }],
        "state should flow into LoadDevice.initial_state: {actions:?}"
    );
}

/// r.md #71: **余剰を落としてから load する** 順序は仕様 (現行 Phase B と同じ)。
/// 逆順だと、 同一 shmem 名を再利用する差し替えで新 mapping を開いた直後に
/// 旧 teardown が走りうる。
#[test]
fn removals_come_before_loads() {
    let song = make_song_with_one_track(15, vec![make_instance(600, "p.new")]);
    let mut loaded_devices = HashMap::new();
    loaded_devices.insert(601, loaded("p.stale"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_devices);
    assert_eq!(
        actions,
        vec![
            SlotReconcileAction::RemoveDevice { device_id: 601 },
            SlotReconcileAction::LoadDevice {
                device_id: 600,
                plugin_id_str: "p.new".into(),
                initial_state: None,
            },
        ],
        "RemoveDevice が LoadDevice より先: {actions:?}"
    );
}
