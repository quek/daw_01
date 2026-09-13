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

/// 帰属トラックが消えたモジュレーター (とそれを使う変調) は、トラック削除と **同じ undo step** で消える
/// (編集後の不変条件が担う)。`*` は削除で立ち、保存時点へ undo すると消えて、モジュレーターも変調も戻る。
#[test]
fn deleting_a_track_reaps_its_modulators_in_the_same_undo_step() {
    use common::model::{AutomationTarget, TrackBuiltinParam};
    let (mut app, _a, _p, _d) = build_app();
    let a = add_track(&mut app);
    let b = add_track(&mut app);
    select(&mut app, b);
    app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Lfo });
    let lfo = app.cur.song_doc.song().mod_sources.last().expect("LFO").id;
    let volume = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
    app.handle_event(AppEvent::AddModRouting { track_id: a, target: volume, source_id: lfo });
    let wired = |app: &AppData| {
        let song = app.cur.song_doc.song();
        (song.mod_sources.iter().map(|m| m.id).collect::<Vec<_>>(), song.all_mod_routings().count())
    };
    assert_eq!(wired(&app), (vec![lfo], 1), "前提: B の LFO が A の音量を変調している");
    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();

    app.handle_event(AppEvent::DeleteTracks(vec![b]));
    assert_eq!(wired(&app), (Vec::new(), 0), "帰属トラックの消えたモジュレーターと、その変調が消える");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "トラック削除と同じ 1 step");
    assert!(app.cur.song_doc.is_dirty());

    app.handle_event(AppEvent::Undo);
    assert_eq!(wired(&app), (vec![lfo], 1), "undo で戻る");
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty(), "保存時点へ戻れば * は消える");
}

/// 貼ったトラックの、宛先が居ない send は外れ、その send の音量のレーンも消える (外すのは貼り付けの
/// 解決、レーンの掃除は同じ編集の中の不変条件)。宛先が居る send とそのレーンは残る。
#[test]
fn pasting_a_track_drops_sends_without_a_destination_with_their_gain_lanes() {
    use common::model::{AutomationLane, AutomationTarget, Send, SendMode, Track, TrackBuiltinParam};
    let (mut app, _a, _p, _d) = build_app();
    let dest = add_track(&mut app);
    let gain = |send_id| AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, legacy_send_idx: None });
    let send = |id, dest_track_id| Send { id, dest_track_id, gain: 1.0, mode: SendMode::PostFader, enabled: true };
    let track = Track {
        id: 900,
        name: "Pasted".into(),
        sends: vec![send(1, 777), send(2, dest)],
        next_send_id: 3,
        automation_lanes: vec![AutomationLane::new(gain(1), 1.0), AutomationLane::new(gain(2), 1.0)],
        ..Track::default()
    };
    let payload = daw_gui::clipboard::TracksCopy {
        tracks: vec![daw_gui::clipboard::TrackCopy { order: 0, track, contents: Vec::new() }],
        scenes: Vec::new(),
    };
    let pid = app.cur.song_doc.song().project_id;
    // 同じプロジェクトとして貼る: 777 は居ない、`dest` は居る。
    assert_eq!(app.paste_tracks_at(payload, pid, dest, &Default::default()), 1);
    let song = app.cur.song_doc.song();
    let pasted = song.tracks.iter().find(|t| t.name == "Pasted").expect("貼ったトラック");
    let kept: Vec<u32> = pasted.sends.iter().map(|s| s.dest_track_id).collect();
    assert_eq!(kept, vec![dest], "宛先が居ない send は外れる");
    let kept_id = pasted.sends[0].id;
    let lanes: Vec<AutomationTarget> = pasted.automation_lanes.iter().map(|l| l.target.clone()).collect();
    assert_eq!(lanes, vec![gain(kept_id)], "外れた send の音量のレーンだけ消える");
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
