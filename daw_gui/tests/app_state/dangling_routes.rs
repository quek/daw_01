//! トラック / chain を消したとき、それを id で指していた信号経路 (内蔵 Comp の SC / follower の tap / send) と
//! MIDI binding が、削除の口ではなく SongDoc の編集後の不変条件 (`Song::prune_dangling_routes`) で掃除される
//! ことを、実際の削除イベントから確かめる。掃除は削除と同じ undo step なので、undo で配線ごと戻る。

use common::model::{
    BindingTarget, ChainRef, MidiBindInput, MidiBinding, NativeKind, TapSource,
};

use daw_gui::app::{AppData, AppEvent, ModSourceKindTag};
use daw_gui::event_device::DeviceEvent;
use daw_gui::widgets::select_modifier::SelectModifier;

use super::support::build_app;

fn add_track(app: &mut AppData) -> u32 {
    let before: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|id| !before.contains(id)).expect("新トラック")
}

fn select(app: &mut AppData, track: u32) {
    let visible: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.apply_select_tracks(track, SelectModifier::Single, &visible);
}

fn comp_sc(app: &AppData, comp: u64) -> Option<TapSource> {
    app.cur.song_doc.song().native_by_id(comp).and_then(|n| n.aux_input).map(|r| r.tap.source)
}

fn follower_source(app: &AppData, id: u32) -> Option<TapSource> {
    let song = app.cur.song_doc.song();
    let m = song.mod_sources.iter().find(|m| m.id == id).expect("follower は残る");
    m.follower().expect("follower").0.map(|t| t.source)
}

/// トラック B を消すと、A の内蔵 Comp の SC / A の follower の tap は入力なし、C → B の send と B の音量の
/// MIDI binding は削除。undo で全部戻り、redo でもう一度消える。
#[test]
fn deleting_a_track_clears_routes_that_read_it_and_undo_restores_them() {
    let (mut app, _a, _p, _d) = build_app();
    let a = add_track(&mut app);
    let b = add_track(&mut app);
    let comp = app.cur.song_doc.song().builtin_native(a, NativeKind::Comp).expect("組み込み Comp").id;
    app.handle_event(AppEvent::Device(DeviceEvent::SetSidechainSource {
        device_id: comp,
        port: 0,
        source: Some(TapSource::Track(b)),
    }));
    select(&mut app, a);
    app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Follower });
    let follower = app.cur.song_doc.song().mod_sources.last().expect("follower").id;
    app.handle_event(AppEvent::SetModSourceTap { id: follower, source: Some(TapSource::Track(b)) });
    // A は B を読む (SC) ので A → B の send は循環して拒否される。send は別のトラック C から張る。
    let c = add_track(&mut app);
    app.handle_event(AppEvent::AddSend { src_track_id: c, dest_track_id: b });
    app.edit_song(|s| {
        s.midi_bindings.push(MidiBinding {
            channel: 0,
            input: MidiBindInput::ControlChange(7),
            legacy_controller: None,
            target: BindingTarget::TrackVolume(b),
        })
    });
    let wired = |app: &AppData| {
        let song = app.cur.song_doc.song();
        (
            comp_sc(app, comp),
            follower_source(app, follower),
            song.track_by_id(c).expect("C").sends.iter().map(|s| s.dest_track_id).collect::<Vec<_>>(),
            song.midi_bindings.iter().map(|m| m.target).collect::<Vec<_>>(),
        )
    };
    let before = wired(&app);
    assert_eq!(before.0, Some(TapSource::Track(b)), "前提: SC");
    assert_eq!(before.1, Some(TapSource::Track(b)), "前提: follower");
    assert_eq!(before.2, vec![b], "前提: send");
    assert_eq!(before.3, vec![BindingTarget::TrackVolume(b)], "前提: binding");

    app.handle_event(AppEvent::DeleteTracks(vec![b]));
    assert!(app.cur.song_doc.song().track_by_id(b).is_none(), "B は消える");
    assert_eq!(wired(&app), (None, None, Vec::new(), Vec::new()), "B を読む配線は全部外れる");

    app.handle_event(AppEvent::Undo);
    assert_eq!(wired(&app), before, "削除と同じ undo step なので配線ごと戻る");
    app.handle_event(AppEvent::Redo);
    assert_eq!(wired(&app), (None, None, Vec::new(), Vec::new()), "redo でもう一度外れる");
}

/// Parallel の chain を消すと、その chain を読む SC は入力なしになる (Comp 本体は残る)。
#[test]
fn removing_a_chain_clears_the_sidechain_that_reads_it() {
    let (mut app, _a, _p, _d) = build_app();
    let a = add_track(&mut app);
    let b = add_track(&mut app);
    select(&mut app, b);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: common::plugin_db::PARALLEL_PICKER_ID.into(),
        keep_open: false,
        open_gui: true,
    });
    let song = app.cur.song_doc.song();
    let parallel = song.fx_chain_by_track_id(b).expect("B").iter().find_map(|d| d.as_parallel()).expect("Parallel").id;
    let chain = song.parallel_by_id(parallel).expect("Parallel").chains[0].id;
    let comp = song.builtin_native(a, NativeKind::Comp).expect("組み込み Comp").id;
    app.handle_event(AppEvent::Device(DeviceEvent::SetSidechainSource {
        device_id: comp,
        port: 0,
        source: Some(TapSource::Chain(chain)),
    }));
    assert_eq!(comp_sc(&app, comp), Some(TapSource::Chain(chain)), "前提: chain を読む SC");

    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![chain] }));
    assert!(app.cur.song_doc.song().chain_devices(ChainRef::Chain(chain)).is_none(), "chain は消える");
    assert_eq!(comp_sc(&app, comp), None, "消えた chain を読む SC は入力なし");
    assert!(app.cur.song_doc.song().native_by_id(comp).is_some(), "Comp 本体は残る");
}
