//! r.md #132 残件 (ARA のコピー): **コピーは元と同じ音で鳴る** — どの経路で写した audio の take も、plug-in host の
//! document に初めて現れるとき元の take の Melodyne の編集から始まる。 ここは GUI 側 (コマンド層) の約束を確かめる:
//!
//! - 同じ content を共有する複製 (リンク) は同じ modification (別のトラックへ貼っても id が同じ = host が元の
//!   document の状態を引ける)。
//! - 独立な複製 / 別のプロジェクトからの貼り付け / event の複製は、写した take が元の take を `take_origins` に持ち、
//!   document の spec が元の modification (と元が居るタブ) を運ぶ。
//! - トラックの独立複製は、device のアーカイブの目次を複製した content の id へ移す (載せたときに自分の状態として
//!   restore できる)。
//! - クリップボードへ写すと、写した take の状態を取っておかせる (`SnapshotAraClipboard`)。
//!
//! どの状態から始めるかの決め方は `daw_plugin_host::ara::graph_plan` の unit test、アーカイブの目次の読み書きは
//! `common` の test が持つ。

use std::collections::HashMap;
use std::sync::Arc;

use common::ara_ids::{AraArchiveEntry, modification_id, origin_modification_id, source_id, take_modification_id};
use common::model::{
    AudioContent, AudioEvent, AudioSource, AudioSourcePath, Clip, ClipContent, ClipKey, Device, LaneRef, PluginInstance,
    StretchMode, TakeOrigin, Track,
};
use common::plugin_db::{CLAP_FEATURE_ARA_SUPPORTED, PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::{AraArchive, AraClipSpec, PluginCommand, PluginEvent, ProjectKey, SlotState};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::clipboard::{ClipboardEnvelope, ClipboardPayload};
use daw_gui::event_tabs::TabEvent;

use super::support::{build_app, drain};

const ARA_PLUGIN: &str = "test.melodyne";
const TRACK_A: u32 = 1;
const TRACK_B: u32 = 2;
const DEVICE_A: u64 = 901;
const DEVICE_B: u64 = 902;
const CLIP_A: ClipKey = ClipKey { track_id: TRACK_A, clip_id: 1 };

/// ARA の plug-in 1 つだけの DB。
fn ara_db() -> Arc<PluginDatabase> {
    let entry = PluginEntry {
        id: ARA_PLUGIN.into(),
        format: PluginFormat::Vst3,
        name: "Test ARA".into(),
        vendor: "Test".into(),
        version: "1.0".into(),
        features: vec![CLAP_FEATURE_ARA_SUPPORTED.into(), "audio-effect".into()],
        path: "C:/fake/ara.vst3".into(),
        descriptor_index: 0,
        has_note_input: false,
        has_note_output: false,
        has_audio_output: true,
        has_audio_input: true,
        has_video_input: false,
        has_video_output: false,
    };
    Arc::new(PluginDatabase::new(vec![entry], None, 0))
}

fn melodyne(id: u64) -> Device {
    Device::Plugin(PluginInstance { id, ..PluginInstance::new(ARA_PLUGIN.into(), PluginFormat::Vst3) })
}

/// 今のタブを「Melodyne を挿したトラック A (8 拍の audio クリップ 1 つ、素材 1) と、Melodyne を挿した空のトラック B」
/// にする。 戻り値 = A のクリップの content。
fn setup_ara_tracks(app: &mut AppData) -> u32 {
    app.ipc.plugin_db = Some(ara_db());
    app.edit_song(|song| {
        song.media.audio_sources.insert(
            1,
            AudioSource {
                path: AudioSourcePath::Absolute("C:/ara-copy/voice.wav".into()),
                sample_rate: 48_000,
                channels: 1,
                frames: 240_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let event = AudioEvent {
            id: 1,
            source_id: 1,
            event_length_beats: 8.0,
            source_end_frames: 192_000,
            stretch_mode: StretchMode::Repitch,
            ..AudioEvent::default()
        };
        let content_id =
            song.alloc_content(ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), String::new());
        song.tracks = vec![
            Track {
                id: TRACK_A,
                clips: vec![Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id, ..Clip::default() }],
                next_clip_id: 100,
                devices: vec![melodyne(DEVICE_A)],
                ..Track::default()
            },
            Track { id: TRACK_B, next_clip_id: 100, devices: vec![melodyne(DEVICE_B)], ..Track::default() },
        ];
        song.ids.next_track_id = 3;
        song.ids.next_device_id = 1000;
        content_id
    })
    .expect("setup")
}

fn ara_app() -> (AppData, UnboundedReceiver<PluginCommand>, u32) {
    let (mut app, _audio, plugin_rx, _d) = build_app();
    let content = setup_ara_tracks(&mut app);
    (app, plugin_rx, content)
}

/// 子プロセスへ流して、device ごとに最後に送った document (作り直し) を返す。 ほかのコマンドは `other` へ。
fn sync(app: &mut AppData, rx: &mut UnboundedReceiver<PluginCommand>, other: &mut Vec<PluginCommand>) -> HashMap<u64, Vec<AraClipSpec>> {
    app.flush_song_sync();
    let mut docs = HashMap::new();
    for msg in drain(rx) {
        match msg {
            PluginCommand::SetupAraDocument { device, clips, .. } => {
                docs.insert(device.device_id, clips);
            }
            msg => other.push(msg),
        }
    }
    docs
}

/// spec の写した元を `(タブ, project_id, modification id)` で。
fn origins(spec: &AraClipSpec) -> Vec<(Option<ProjectKey>, u64, String)> {
    spec.modification_origins.iter().map(|o| (o.project, o.project_id, o.modification_id.clone())).collect()
}

fn content_events(app: &AppData, content: u32) -> Vec<AudioEvent> {
    app.cur.song_doc.song().clip_contents[&content].audio_events().expect("audio").to_vec()
}

fn clip_on(app: &AppData, track: u32) -> Clip {
    app.cur.song_doc.song().track_by_id(track).expect("track").clips.first().expect("clip").clone()
}

/// クリップを別のトラックへ貼る: 同じプロジェクトで元の content が居ればリンク (同じ modification = host は元の
/// document の今の状態を引く)、クリップボードへ写した時点で写した take の状態を取っておかせる。
#[test]
fn 別のトラックへ貼ったクリップは元と同じ_modification_で写した時点の状態を取っておく() {
    let (mut app, mut rx, content) = ara_app();
    let mut other = Vec::new();
    let before = sync(&mut app, &mut rx, &mut other);
    let original = before[&DEVICE_A][0].clone();
    assert_eq!(original.modification_id, take_modification_id(content, 1, 1), "前提");

    let (envelope, _, _) = app.clips_copy_envelope(&[CLIP_A]).expect("copy");
    let json = envelope.to_json().expect("json");
    app.snapshot_ara_for_clipboard(&json);
    let project_id = app.cur.song_doc.song().project_id;
    let snapshots: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter_map(|m| match m {
            PluginCommand::SnapshotAraClipboard { project, project_id, modifications } => Some((project, project_id, modifications)),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, vec![(app.pk(), project_id, vec![original.modification_id.clone()])], "写した take の状態を取っておく");

    let ClipboardPayload::Clips(clips) = ClipboardEnvelope::from_json(&json).expect("envelope").payload else { panic!("clips") };
    assert_eq!(app.paste_clips_at(clips, project_id, TRACK_B, 16.0, &envelope.media), 1);
    let docs = sync(&mut app, &mut rx, &mut other);
    let pasted = &docs[&DEVICE_B][0];
    assert_eq!(pasted.modification_id, original.modification_id, "リンクは同じ modification (元の document の状態を引く)");
    assert!(!docs.contains_key(&DEVICE_A), "元の document は作り直さない");
}

/// 写したクリップの take が複製 (元の take を指す) なら、クリップボードへ写すときその祖先の状態も取っておかせる — 複製が
/// ARA トラックに載っていない (document に状態が無い) とき、元のプロジェクトを閉じてから貼っても祖先の編集から始まる。
#[test]
fn クリップボードへ写すと写した_take_の祖先の状態も取っておく() {
    let (mut app, mut rx, content) = ara_app();
    let project_id = app.cur.song_doc.song().project_id;
    app.edit_song(|song| song.tracks[1].devices.clear()).expect("B を ARA でないトラックにする");
    app.copy_time_range(0.0, 8.0, 8.0, &[(TRACK_A, TRACK_B)], true);
    let copy = clip_on(&app, TRACK_B);
    let _ = drain(&mut rx);

    let (envelope, _, _) = app.clips_copy_envelope(&[ClipKey { track_id: TRACK_B, clip_id: copy.id }]).expect("copy");
    app.snapshot_ara_for_clipboard(&envelope.to_json().expect("json"));
    let snapshot = drain(&mut rx).into_iter().find_map(|m| match m {
        PluginCommand::SnapshotAraClipboard { project_id: p, mut modifications, .. } if p == project_id => {
            modifications.sort();
            Some(modifications)
        }
        _ => None,
    });
    let mut expected = vec![take_modification_id(copy.content_id, 1, 1), take_modification_id(content, 1, 1)];
    expected.sort();
    assert_eq!(snapshot, Some(expected), "写した take (ARA トラックに居ない) と、その元の take");
}

/// 無効のトラック (plug-in host に document が無い) から有効な Melodyne トラックへ移したクリップは、無効のトラックの
/// device の保存したアーカイブにしか編集が無いので、組み直す document より先にそのアーカイブを host へ預ける。 移し元の
/// document が host に居る (有効なトラック) なら預けない (host が生きている document から引く)。
#[test]
fn 無効のトラックから移したクリップは移し元の保存したアーカイブを先に預ける() {
    let setup = |enabled: bool| {
        let (mut app, mut rx, content) = ara_app();
        let modification = take_modification_id(content, 1, 1);
        app.edit_song(|song| {
            song.tracks[0].enabled = enabled;
            let Some(Device::Plugin(melodyne)) = song.tracks[0].devices.first_mut() else { panic!("device") };
            melodyne.set_ara_archive(Arc::from(&b"track-a"[..]), vec![source_id(1), modification.clone()]);
        })
        .expect("A の保存したアーカイブ");
        let mut other = Vec::new();
        sync(&mut app, &mut rx, &mut other);
        let (envelope, _, _) = app.clips_copy_envelope(&[CLIP_A]).expect("copy");
        let ClipboardPayload::Clips(clips) = envelope.payload.clone() else { panic!("clips") };
        let project_id = app.cur.song_doc.song().project_id;
        assert_eq!(app.paste_clips_at(clips, project_id, TRACK_B, 16.0, &envelope.media), 1);
        app.flush_song_sync();
        (drain(&mut rx), modification, app)
    };

    let (sent, modification, app) = setup(false);
    let order: Vec<&str> = sent
        .iter()
        .filter_map(|m| match m {
            PluginCommand::KeepDormantAraArchive { device, plugin_id, archive, archive_ids } => {
                assert_eq!((device.device_id, plugin_id.as_str(), archive.as_slice()), (DEVICE_A, ARA_PLUGIN, &b"track-a"[..]));
                assert_eq!(common::ara_ids::archived_id(archive_ids, &modification), Some(modification.as_str()));
                assert_eq!(device.project, app.pk());
                Some("dormant")
            }
            PluginCommand::SetupAraDocument { device, clips, .. } if device.device_id == DEVICE_B => {
                assert_eq!(clips[0].modification_id, modification, "移したクリップは同じ modification");
                Some("setup")
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["dormant", "setup"], "移し元のアーカイブを先に預ける");

    let (sent, _, _) = setup(true);
    assert!(
        !sent.iter().any(|m| matches!(m, PluginCommand::KeepDormantAraArchive { .. })),
        "移し元の document が host に居れば預けない"
    );
}

/// 無効のトラックのクリップを別のタブへ貼る / クリップボードへ写すときも、そのトラックの device の保存したアーカイブを先に
/// 預ける (host は元のタブの device として持ち、貼った take / クリップボードの写しがそこから始まる)。
#[test]
fn 無効のトラックのクリップを別のタブへ貼る_写すときも保存したアーカイブを預ける() {
    let (mut app, mut rx, content) = ara_app();
    let source_tab = app.pk();
    let modification = take_modification_id(content, 1, 1);
    app.edit_song(|song| {
        song.tracks[0].enabled = false;
        let Some(Device::Plugin(melodyne)) = song.tracks[0].devices.first_mut() else { panic!("device") };
        melodyne.set_ara_archive(Arc::from(&b"track-a"[..]), vec![source_id(1), modification.clone()]);
    })
    .expect("A を無効にして保存したアーカイブを持たせる");
    let mut other = Vec::new();
    sync(&mut app, &mut rx, &mut other);
    let dormant_devices = |sent: &[PluginCommand]| -> Vec<(ProjectKey, u64)> {
        sent.iter()
            .filter_map(|m| match m {
                PluginCommand::KeepDormantAraArchive { device, .. } => Some((device.project, device.device_id)),
                _ => None,
            })
            .collect()
    };

    let (envelope, _, _) = app.clips_copy_envelope(&[CLIP_A]).expect("copy");
    app.snapshot_ara_for_clipboard(&envelope.to_json().expect("json"));
    let sent = drain(&mut rx);
    assert_eq!(dormant_devices(&sent), vec![(source_tab, DEVICE_A)], "写す take の状態はそのアーカイブにしか無い");
    assert!(matches!(sent.last(), Some(PluginCommand::SnapshotAraClipboard { .. })), "預けてから写しを取っておかせる");

    app.handle_event(AppEvent::Tab(TabEvent::New));
    setup_ara_tracks(&mut app);
    let _ = drain(&mut rx);
    let ClipboardPayload::Clips(clips) = envelope.payload.clone() else { panic!("clips") };
    assert_eq!(app.paste_clips_at(clips, envelope.source_project_id, TRACK_B, 0.0, &envelope.media), 1);
    app.flush_song_sync();
    assert_eq!(dormant_devices(&drain(&mut rx)), vec![(source_tab, DEVICE_A)], "元のタブの無効のトラックの device として預ける");
}

/// 範囲を別のトラックへ **独立に** 複製すると、写した take は別の modification になり、元の modification (同じタブ) から
/// 始める。 リンクの複製は同じ modification のまま。
#[test]
fn 範囲の独立な複製は写した_take_が元の_take_を指しリンクは同じ_modification() {
    let (mut app, mut rx, content) = ara_app();
    let mut other = Vec::new();
    let original = sync(&mut app, &mut rx, &mut other)[&DEVICE_A][0].modification_id.clone();
    let project_id = app.cur.song_doc.song().project_id;

    app.copy_time_range(0.0, 8.0, 8.0, &[(TRACK_A, TRACK_B)], true);
    let copy = clip_on(&app, TRACK_B);
    assert_ne!(copy.content_id, content);
    let docs = sync(&mut app, &mut rx, &mut other);
    let unique = &docs[&DEVICE_B][0];
    assert_ne!(unique.modification_id, original, "独立な複製は別の modification");
    assert_eq!(origins(unique), vec![(Some(app.pk()), project_id, original.clone())], "元の take の編集から始める");

    app.handle_event(AppEvent::Undo);
    app.copy_time_range(0.0, 8.0, 8.0, &[(TRACK_A, TRACK_B)], false);
    let docs = sync(&mut app, &mut rx, &mut other);
    let linked = &docs[&DEVICE_B][0];
    assert_eq!((linked.modification_id.as_str(), linked.modification_origins.len()), (original.as_str(), 0));
}

/// 別のタブへ貼ったクリップは、元のタブの modification を指す (host がそのタブの document から写す)。 元のプロジェクトが
/// 開いていない (閉じた / 別のアプリの) クリップボードからは、タブを持たずに `project_id` だけで指す (写した時点の状態)。
#[test]
fn 別のプロジェクトから貼った_take_は元のプロジェクトの_modification_を指す() {
    let (mut app, mut rx, content) = ara_app();
    let source_tab = app.pk();
    let source_project = app.cur.song_doc.song().project_id;
    let (envelope, _, _) = app.clips_copy_envelope(&[CLIP_A]).expect("copy");
    let original = take_modification_id(content, 1, 1);

    let paste = |app: &mut AppData, envelope: &ClipboardEnvelope| {
        let ClipboardPayload::Clips(clips) = envelope.payload.clone() else { panic!("clips") };
        app.paste_clips_at(clips, envelope.source_project_id, TRACK_B, 0.0, &envelope.media)
    };

    app.handle_event(AppEvent::Tab(TabEvent::New));
    assert_ne!(app.pk(), source_tab);
    setup_ara_tracks(&mut app);
    assert_ne!(app.cur.song_doc.song().project_id, source_project, "前提: 別のプロジェクト");
    let _ = drain(&mut rx);
    assert_eq!(paste(&mut app, &envelope), 1);
    let mut other = Vec::new();
    let docs = sync(&mut app, &mut rx, &mut other);
    let pasted = &docs[&DEVICE_B][0];
    assert_eq!(origins(pasted), vec![(Some(source_tab), source_project, original.clone())], "元のタブの document から写す");

    let (mut closed, mut closed_rx, _) = ara_app();
    let _ = drain(&mut closed_rx);
    assert_eq!(paste(&mut closed, &envelope), 1);
    let docs = sync(&mut closed, &mut closed_rx, &mut other);
    assert_eq!(origins(&docs[&DEVICE_B][0]), vec![(None, source_project, original)], "開いていないプロジェクトはクリップボードの写しだけ");
}

/// オーディオエディタで event を複製すると、複製した take は元の take (同じ content) を指す。
#[test]
fn 複製した_audio_event_は元の_take_の編集から始める() {
    let (mut app, mut rx, content) = ara_app();
    let project_id = app.cur.song_doc.song().project_id;
    app.handle_event(AppEvent::OpenAudioEditor(CLIP_A));
    app.handle_event(AppEvent::SetTimeSelection { start_beat: 0.0, end_beat: 8.0, lanes: vec![LaneRef::AudioLane(CLIP_A)] });
    app.handle_event(AppEvent::DuplicateAudioEditorEvent);
    let events = content_events(&app, content);
    assert_eq!(events.len(), 2, "前提: 複製した");
    let copy = &events[1];
    assert_ne!(copy.take_key(), 1, "別の take");
    assert_eq!(copy.take_origins, vec![TakeOrigin { project_id, content, take: 1, source: 1 }]);
    let mut other = Vec::new();
    let docs = sync(&mut app, &mut rx, &mut other);
    let spec = docs[&DEVICE_A].iter().find(|s| s.modification_id == modification_id(content, copy)).expect("複製した take の region");
    assert_eq!(origins(spec), vec![(Some(app.pk()), project_id, take_modification_id(content, 1, 1))]);
}

/// トラックを独立に複製すると、複製した device のアーカイブの目次は複製した content の id を指す (書かれている id は元の
/// take) — 載せたときに自分の状態として restore できる。 複製した take は元の take も指す。
#[test]
fn 独立に複製したトラックの_device_はアーカイブの目次を複製した_content_へ移す() {
    let (mut app, mut rx, content) = ara_app();
    let original = take_modification_id(content, 1, 1);
    app.handle_event(AppEvent::DuplicateTracksUnique(vec![TRACK_A]));
    assert!(drain(&mut rx).iter().any(|m| matches!(m, PluginCommand::RequestAllStates { .. })), "前提: 最新の state を取り寄せる");
    let archive = AraArchive { bytes: b"melodyne".to_vec(), ids: vec![source_id(1), original.clone()] };
    let entries = vec![SlotState { device_id: DEVICE_A, data: None, ara_archive: Some(archive), error: None }];
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries }));

    let song = app.cur.song_doc.song();
    assert_eq!(song.tracks.len(), 3, "複製した");
    let duplicate = &song.tracks[1];
    let copy_content = duplicate.clips[0].content_id;
    assert_ne!(copy_content, content);
    let Some(Device::Plugin(device)) = duplicate.devices.first() else { panic!("device") };
    let copied = take_modification_id(copy_content, 1, 1);
    assert_eq!(
        device.ara_archive_ids,
        vec![AraArchiveEntry::stored(source_id(1)), AraArchiveEntry { current: copied.clone(), archived: Some(original.clone()) }]
    );
    assert_eq!(
        song.plugin_by_id(DEVICE_A).expect("original").ara_archive_ids,
        vec![AraArchiveEntry::stored(source_id(1)), AraArchiveEntry::stored(original.clone())],
        "元の device は plug-in host が書いた目次のまま"
    );
    let device_id = device.id;
    let copy_events = content_events(&app, copy_content);
    assert_eq!(copy_events[0].take_origins.iter().map(origin_modification_id).collect::<Vec<_>>(), vec![original.clone()]);

    // 載せた device の document は、目次を移したアーカイブと、元の take を指す spec で組む。
    app.flush_song_sync();
    let setup = drain(&mut rx).into_iter().find_map(|m| match m {
        PluginCommand::SetupAraDocument { device, clips, archive, archive_ids, .. } if device.device_id == device_id => {
            Some((clips, archive, archive_ids))
        }
        _ => None,
    });
    let (clips, archive, archive_ids) = setup.expect("複製した device の document");
    assert_eq!(archive.as_deref(), Some(&b"melodyne"[..]));
    assert_eq!(archived_of(&archive_ids, &copied), Some(original.as_str()));
    assert_eq!(clips[0].modification_id, copied);
    assert_eq!(clips[0].modification_origins.iter().map(|o| o.modification_id.clone()).collect::<Vec<_>>(), vec![original]);
}

fn archived_of<'a>(entries: &'a [AraArchiveEntry], current: &str) -> Option<&'a str> {
    common::ara_ids::archived_id(entries, current)
}

/// ランチャーのセルの独立な複製は、写した take が元の take を指す (アレンジへ運んでも元の編集から始まる)。
#[test]
fn 独立に複製したセルの_take_は元の_take_を指す() {
    use daw_gui::event_launcher::{LauncherCellKey, LauncherEvent};
    let (mut app, _rx, content) = ara_app();
    let project_id = app.cur.song_doc.song().project_id;
    app.edit_song(|song| {
        let scene = song.alloc_scene_id();
        song.scenes.push(common::model::Scene::new(scene));
        let cell = common::model::SessionClip {
            scene_id: scene,
            clip: Clip { id: 50, length_beats: 8.0, content_id: content, ..Clip::default() },
            launch: common::model::LaunchSettings::default(),
        };
        song.tracks[0].session_clips.push(cell);
    });
    let cell = LauncherCellKey::Track(ClipKey { track_id: TRACK_A, clip_id: 50 });
    app.handle_event(AppEvent::Launcher(LauncherEvent::DuplicateCells { cells: vec![cell], unique: true }));
    let track = app.cur.song_doc.song().track_by_id(TRACK_A).expect("track");
    let copy = track.session_clips.iter().find(|c| c.clip.id != 50).expect("複製したセル").clip.content_id;
    assert_ne!(copy, content);
    let events = content_events(&app, copy);
    assert_eq!(events[0].take_origins, vec![TakeOrigin { project_id, content, take: 1, source: 1 }]);
}
