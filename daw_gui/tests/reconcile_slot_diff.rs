// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `compute_slot_reconcile_actions` の device-level diff を unit test で
//! 固定する。
//!
//! 単一デバイスチェーン (`docs/plan_linear_chain.md`): 役割別 3 chain を捨て、
//! `Track.devices` を flat な `index: u32` 空間で diff する。3 ケース
//! (host extra / song extra / plugin_id_str mismatch) + 完全一致 + state 伝搬を
//! pure function 経由で固定する。

use std::collections::HashMap;

use common::model::{PluginInstance, Song};
use common::plugin_format::PluginFormat;

use daw_gui::app::{LoadedSlotInfo, SlotReconcileAction, compute_slot_reconcile_actions};

fn make_instance(plugin_id: &str) -> PluginInstance {
    PluginInstance::new(plugin_id.into(), PluginFormat::Clap)
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

fn loaded(device_id: u64, plugin_id_str: &str) -> LoadedSlotInfo {
    LoadedSlotInfo {
        device_id,
        plugin_id_str: plugin_id_str.into(),
    }
}

#[test]
fn host_extra_slot_yields_remove_action() {
    // host に device 0 + 1 が居る、 song は device 0 のみ → device 1 を消す。
    let track_id = 10;
    let song = make_song_with_one_track(track_id, vec![make_instance("p.comp")]);
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, 0), loaded(100, "p.comp"));
    loaded_slots.insert((track_id, 1), loaded(101, "p.reverb"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::RemoveSlot {
            track_id,
            index: 1,
        }],
        "host extra device 1 should be removed: {actions:?}"
    );
}

#[test]
fn song_extra_slot_yields_load_action() {
    // host に device 0 のみ、 song は device 0 + 1 → device 1 を load。
    let track_id = 11;
    let song = make_song_with_one_track(
        track_id,
        vec![make_instance("p.comp"), make_instance("p.reverb")],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, 0), loaded(200, "p.comp"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            index: 1,
            plugin_id_str: "p.reverb".into(),
            initial_state: None,
        }],
        "song extra device 1 should be loaded: {actions:?}"
    );
}

#[test]
fn plugin_id_mismatch_yields_load_action() {
    // host に device 0 = PluginA、 song は device 0 = PluginB → Load (= 入れ替え)。
    // plugin_host 側の SetSlotPlugin handler が dedup logic で
    // 「同 index に違う plugin」 を見て unload + load を組み立てる前提。
    let track_id = 12;
    let song = make_song_with_one_track(track_id, vec![make_instance("p.B")]);
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, 0), loaded(300, "p.A"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            index: 0,
            plugin_id_str: "p.B".into(),
            initial_state: None,
        }],
        "device 0 plugin_id_str mismatch should trigger LoadSlot: {actions:?}"
    );
}

#[test]
fn matching_slot_produces_no_action() {
    // host と song が完全一致 → action 空。
    let track_id = 13;
    let song = make_song_with_one_track(
        track_id,
        vec![
            make_instance("p.midi"),
            make_instance("p.synth"),
            make_instance("p.comp"),
        ],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, 0), loaded(402, "p.midi"));
    loaded_slots.insert((track_id, 1), loaded(400, "p.synth"));
    loaded_slots.insert((track_id, 2), loaded(401, "p.comp"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert!(
        actions.is_empty(),
        "perfectly synced track yields no actions: {actions:?}"
    );
}

#[test]
fn initial_state_propagates_to_load_action() {
    // Song.PluginInstance::state が LoadAction::initial_state に伝搬する
    // ことを確認 (= Undo で knob 値復元の根幹)。
    let track_id = 14;
    let mut inst = make_instance("p.synth");
    inst.state = Some(vec![1, 2, 3, 4].into());
    let song = make_song_with_one_track(track_id, vec![inst]);
    let loaded_slots = HashMap::new();

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            index: 0,
            plugin_id_str: "p.synth".into(),
            initial_state: Some(vec![1, 2, 3, 4]),
        }],
        "state should flow into LoadSlot.initial_state: {actions:?}"
    );
}
