//! `compute_slot_reconcile_actions` の slot-level diff を unit test で
//! 固定する。
//!
//! Risk D (plan_undo_reconcile_polish.md):
//! 4dc982c で「Undo/Redo reconcile を slot 粒度に拡張」 した動作は実機 +
//! `spec/sidechain.daw` smoke でしか確認していなかった。 本 test で
//! 3 ケース (host extra / song extra / plugin_id_str mismatch) を pure
//! function 経由で固定する。

use std::collections::HashMap;

use common::model::{PluginInstance, Song, Track};
use common::plugin_format::PluginFormat;
use common::protocol::PluginSlot;

use daw_gui::app::{LoadedSlotInfo, SlotReconcileAction, compute_slot_reconcile_actions};

fn make_instance(plugin_id: &str) -> PluginInstance {
    PluginInstance::new(plugin_id.into(), PluginFormat::Clap)
}

fn make_song_with_one_track(
    track_id: u32,
    instrument: Option<PluginInstance>,
    fx: Vec<PluginInstance>,
    midi_fx: Vec<PluginInstance>,
) -> Song {
    let mut song = Song::default();
    song.tracks.push(Track {
        id: track_id,
        name: "T".into(),
        instrument,
        fx_chain: fx,
        midi_fx_chain: midi_fx,
        ..Default::default()
    });
    song
}

fn loaded(plugin_id: u32, plugin_id_str: &str) -> LoadedSlotInfo {
    LoadedSlotInfo {
        plugin_id,
        plugin_id_str: plugin_id_str.into(),
    }
}

#[test]
fn host_extra_slot_yields_remove_action() {
    // host に Fx(0) + Fx(1) が居る、 song は Fx(0) のみ → Fx(1) を消す。
    let track_id = 10;
    let song = make_song_with_one_track(
        track_id,
        None,
        vec![make_instance("p.comp")],
        vec![],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, PluginSlot::Fx(0)), loaded(100, "p.comp"));
    loaded_slots.insert((track_id, PluginSlot::Fx(1)), loaded(101, "p.reverb"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::RemoveSlot {
            track_id,
            slot: PluginSlot::Fx(1),
        }],
        "host extra Fx(1) should be removed: {actions:?}"
    );
}

#[test]
fn song_extra_slot_yields_load_action() {
    // host に Fx(0) のみ、 song は Fx(0) + Fx(1) → Fx(1) を load。
    let track_id = 11;
    let song = make_song_with_one_track(
        track_id,
        None,
        vec![make_instance("p.comp"), make_instance("p.reverb")],
        vec![],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, PluginSlot::Fx(0)), loaded(200, "p.comp"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            slot: PluginSlot::Fx(1),
            plugin_id_str: "p.reverb".into(),
            initial_state: None,
        }],
        "song extra Fx(1) should be loaded: {actions:?}"
    );
}

#[test]
fn plugin_id_mismatch_yields_load_action() {
    // host に Fx(0)=PluginA、 song は Fx(0)=PluginB → Load (= 入れ替え)。
    // plugin_host 側の SetSlotPlugin handler が dedup logic で
    // 「同 slot に違う plugin」 を見て unload + load を組み立てる前提。
    let track_id = 12;
    let song = make_song_with_one_track(
        track_id,
        None,
        vec![make_instance("p.B")],
        vec![],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, PluginSlot::Fx(0)), loaded(300, "p.A"));

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            slot: PluginSlot::Fx(0),
            plugin_id_str: "p.B".into(),
            initial_state: None,
        }],
        "Fx(0) plugin_id_str mismatch should trigger LoadSlot: {actions:?}"
    );
}

#[test]
fn matching_slot_produces_no_action() {
    // host と song が完全一致 → action 空。
    let track_id = 13;
    let song = make_song_with_one_track(
        track_id,
        Some(make_instance("p.synth")),
        vec![make_instance("p.comp")],
        vec![make_instance("p.midi")],
    );
    let mut loaded_slots = HashMap::new();
    loaded_slots.insert((track_id, PluginSlot::Instrument), loaded(400, "p.synth"));
    loaded_slots.insert((track_id, PluginSlot::Fx(0)), loaded(401, "p.comp"));
    loaded_slots.insert((track_id, PluginSlot::MidiFx(0)), loaded(402, "p.midi"));

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
    inst.state = Some(vec![1, 2, 3, 4]);
    let song = make_song_with_one_track(track_id, Some(inst), vec![], vec![]);
    let loaded_slots = HashMap::new();

    let actions = compute_slot_reconcile_actions(&song, &loaded_slots);
    assert_eq!(
        actions,
        vec![SlotReconcileAction::LoadSlot {
            track_id,
            slot: PluginSlot::Instrument,
            plugin_id_str: "p.synth".into(),
            initial_state: Some(vec![1, 2, 3, 4]),
        }],
        "state should flow into LoadSlot.initial_state: {actions:?}"
    );
}
