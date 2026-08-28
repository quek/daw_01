//! r.md #27 / #28 回帰: Transform (立ち絵 group transform) 等の非 vocal 編集が
//! (a) builtin VOICEVOX の再合成を trigger しないこと (#27)、(b) 1 回の drag が
//! undo 履歴を 1 step だけ増やすこと (#28) を検証する。
//!
//! いずれも root cause は「あらゆる song 編集が epoch を bump し、frame flush が
//! 一律に子プロセス sync を走らせる」設計で、個々の sync step / drag が自分で
//! 差分・gesture bracket を持つべきだった、という抜け。

use common::model::{
    Clip, ClipContent, GroupTransformParam, MidiContent, Note, PluginInstance, Track,
};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::PluginCommand;

use daw_gui::app::{AppEvent, LoadedDeviceInfo};

use super::support::{build_app, drain};

const VOCAL_ID: u32 = 100;
const VOCAL_DEVICE_ID: u64 = 5;

fn note(pitch: u8, start: f64, dur: f64, lyric: &str) -> Note {
    Note {
        id: 0,
        start_beat: start,
        duration_beats: dur,
        pitch,
        velocity: 100,
        lyric: Some(lyric.to_string()),
        muted: false,
    }
}

/// builtin VOICEVOX device (安定 id = `VOCAL_DEVICE_ID`) + notes 入り MIDI clip を
/// 持つ vocal track を app に足し、`loaded_devices` にも登録する (= SlotPluginLoaded 相当、
/// ただし本体の load handler を通さないので metadata cache は clear されない)。
#[allow(clippy::field_reassign_with_default)]
fn add_loaded_vocal_track(app: &mut daw_gui::app::AppData) {
    app.edit_song(|song| {
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![note(60, 0.0, 1.0, "ら"), note(62, 1.0, 1.0, "ら")],
                next_note_id: 3,
            }),
        );
        let mut track = Track::default();
        track.id = VOCAL_ID;
        track.name = "Vocal".into();
        track.devices.push(PluginInstance {
            id: VOCAL_DEVICE_ID,
            ..PluginInstance::with_ports(
                common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                PluginFormat::Builtin,
                PortConfig { has_note_input: true, has_audio_output: true, ..Default::default() },
            )
        });
        let mut clip = Clip::default();
        clip.id = 1;
        clip.length_beats = 4.0;
        clip.content_id = cid;
        track.clips.push(clip);
        song.tracks.push(track);
    });
    app.ipc.loaded_devices.insert(
        VOCAL_DEVICE_ID,
        LoadedDeviceInfo {
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
        },
    );
}

fn count_note_metadata(msgs: &[PluginCommand]) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, PluginCommand::SetBuiltinPluginNoteMetadata { .. }))
        .count()
}

/// r.md #27: Transform param を編集しても VOICEVOX 合成 (= `SetBuiltinPluginNoteMetadata`
/// 再送) が走らない。歌唱入力 (bpm/notes/歌詞) が変わったときだけ再送する。
#[test]
fn transform_edit_does_not_resend_vocal_metadata() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    add_loaded_vocal_track(&mut app);
    let _ = drain(&mut plugin_rx);

    // 初回 flush: cache 空なので metadata を 1 回送る (= seed 合成)。
    app.flush_song_sync();
    let baseline = drain(&mut plugin_rx);
    assert_eq!(
        count_note_metadata(&baseline),
        1,
        "初回 flush は builtin VOICEVOX に metadata を送る (seed): {baseline:?}"
    );

    // Transform param を編集 (= 立ち絵 group transform)。歌唱入力ではないので、
    // flush しても metadata は再送されない (r.md #27 の核心)。
    app.handle_event(AppEvent::SetGroupTransformField {
        track_id: VOCAL_ID,
        param: GroupTransformParam::X,
        value: 0.5,
    });
    app.flush_song_sync();
    let after_transform = drain(&mut plugin_rx);
    assert_eq!(
        count_note_metadata(&after_transform),
        0,
        "Transform 編集では VOICEVOX 合成を trigger しない (metadata 再送なし): {after_transform:?}"
    );

    // 歌唱入力 (bpm) を変えると差分検出で再送する (cache-miss 経路の対の保証)。
    let _ = app.edit_song(|s| s.bpm = 132.0);
    app.flush_song_sync();
    let after_bpm = drain(&mut plugin_rx);
    assert_eq!(
        count_note_metadata(&after_bpm),
        1,
        "bpm 変更は歌唱合成入力なので metadata を再送する: {after_bpm:?}"
    );
}

/// r.md #28: group transform の drag (Begin → 複数 Set → End) は undo 履歴を
/// **1 step だけ** 増やす。arch refactor で bracket が no-op のまま残り per-frame
/// の Set がそれぞれ undo step を積んで履歴を溢れさせていた回帰のガード。
#[test]
fn group_transform_drag_is_one_undo_step() {
    let (mut app, _audio_rx, _plugin_rx, _proxy) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    let before = app.song_doc.undo_depth();

    app.handle_event(AppEvent::BeginGroupTransformDrag);
    for i in 1..=8 {
        app.handle_event(AppEvent::SetGroupTransformField {
            track_id,
            param: GroupTransformParam::X,
            value: 0.05 * i as f32,
        });
    }
    app.handle_event(AppEvent::EndGroupTransformDrag);

    assert_eq!(
        app.song_doc.undo_depth(),
        before + 1,
        "8 フレームぶんの Transform scrub は 1 undo step に bracket される"
    );
    // 1 回の undo で drag 全体が巻き戻る (= 途中値が履歴に残らない)。
    assert!(app.song_doc.can_undo());
    app.handle_event(AppEvent::Undo);
    assert_eq!(
        app.song_doc.undo_depth(),
        before,
        "1 回の undo で drag 前まで戻る"
    );
}
