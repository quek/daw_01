//! r.md #133: トラック・シーンの既定名は数字だけ。
//!
//! 既定名は **「名前を持たない」 状態 (空) として保存** し、 表示は並び順の番号
//! (`Track::display_name` / `Song::track_display_name`、 シーンは `Scene::display_name`) が作る。
//! 焼き込まないので並べ替え・削除に番号が追従し、 改名で空か番号を確定すると未命名に戻る。
//! 既存プロジェクトの `Track 3` のような名前はユーザーが付けた名前と区別できないので書き換えない。

use std::collections::HashSet;

use common::model::MASTER_TRACK_ID;
use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_launcher::LauncherEvent;

use super::support::{self, select_track_single};

#[cfg(windows)]
use common::protocol::PluginCommand;
#[cfg(windows)]
use daw_gui::event_device::DeviceEvent;

fn track_names(app: &AppData) -> Vec<String> {
    app.cur.song_doc.song().tracks.iter().map(|t| t.name.clone()).collect()
}

/// ミキサーのストリップに出る名前 (`TrackMixEntry::name`)。
fn mixer_labels(app: &AppData) -> Vec<String> {
    app.track_mix().into_iter().map(|e| e.name).collect()
}

fn track_ids(app: &AppData) -> Vec<u32> {
    app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect()
}

fn rename_track(app: &mut AppData, track_id: u32, text: &str) {
    app.handle_event(AppEvent::BeginRenameTrack(track_id));
    app.handle_event(AppEvent::RenameTrackChanged(text.to_string()));
    app.handle_event(AppEvent::CommitRenameTrack);
}

fn rename_scene(app: &mut AppData, scene_id: u32, text: &str) {
    app.handle_event(AppEvent::Launcher(LauncherEvent::BeginRenameScene(scene_id)));
    app.handle_event(AppEvent::Launcher(LauncherEvent::RenameSceneChanged(text.to_string())));
    app.handle_event(AppEvent::Launcher(LauncherEvent::CommitRenameScene));
}

#[test]
fn 新しいトラックは未命名で上からの番号を表示し並べ替えと削除に追従する() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.handle_event(AppEvent::AddInstrumentTrack);
    assert_eq!(track_names(&app), ["", "", ""], "新規プロジェクトの 1 本目も、追加したトラックも名前を焼き込まない");
    assert_eq!(mixer_labels(&app), ["1", "2", "3"]);

    let before = track_ids(&app);
    let reversed: Vec<u32> = before.iter().rev().copied().collect();
    app.handle_event(AppEvent::ReorderTracks(reversed.clone()));
    assert_eq!(track_ids(&app), reversed);
    assert_eq!(app.cur.song_doc.song().track_display_name(before[0]), "3", "一番上の行を一番下へ動かすと 3");
    assert_eq!(mixer_labels(&app), ["1", "2", "3"], "番号は行の位置に付く");

    app.handle_event(AppEvent::DeleteTracks(vec![reversed[1]]));
    let song = app.cur.song_doc.song();
    assert_eq!(mixer_labels(&app), ["1", "2"]);
    assert_eq!(song.track_display_name(reversed[2]), "2", "上の行を消すと番号が詰まる");
    assert_eq!(song.track_display_name(reversed[1]), "(削除済み)");
    assert_eq!(song.track_display_name(MASTER_TRACK_ID), "Master");
}

#[test]
fn グループ化と送り先追加も未命名で作りグループも送り先も子も通し番号で数える() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let children = track_ids(&app);
    app.handle_event(AppEvent::GroupSelectedTracks { track_ids: children.clone() });
    select_track_single(&mut app, 1);
    app.handle_event(AppEvent::AddSendToNewTrack { src_track_id: children[0] });

    let song = app.cur.song_doc.song();
    assert_eq!(song.tracks.len(), 4, "グループ + 子 2 本 + 送り先");
    assert_eq!(track_names(&app), ["", "", "", ""], "グループも送り先も名前を焼き込まない");
    let mix = app.track_mix();
    let rows: Vec<(&str, bool)> = mix.iter().map(|e| (e.name.as_str(), e.is_group)).collect();
    assert_eq!(
        rows,
        [("1", true), ("2", false), ("3", false), ("4", false)],
        "上からの通し番号 (グループの子も続きの番号)"
    );
    select_track_single(&mut app, 2);
    assert_eq!(app.selected_track_label(), "3", "インスペクタの見出しも同じ番号");
}

#[test]
fn 改名は空か自動名の番号で確定すると未命名に戻りトラックと列で同じ規則() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let id = app.cur.song_doc.song().tracks[1].id;
    let name = |app: &AppData| app.cur.song_doc.song().track_by_id(id).map(|t| t.name.clone());

    app.handle_event(AppEvent::BeginRenameTrack(id));
    assert_eq!(app.cur.peph.track_rename_text, "2", "未命名の入力欄の初期値は表示中の番号");
    app.handle_event(AppEvent::CancelRenameTrack);

    rename_track(&mut app, id, " Bass ");
    assert_eq!(name(&app).as_deref(), Some("Bass"));
    app.handle_event(AppEvent::BeginRenameTrack(id));
    assert_eq!(app.cur.peph.track_rename_text, "Bass", "名前付きの初期値はその名前");
    app.handle_event(AppEvent::CancelRenameTrack);

    rename_track(&mut app, id, "2");
    assert_eq!(name(&app).as_deref(), Some(""), "いまの位置の番号を確定すると未命名に戻る");
    rename_track(&mut app, id, "Bass");
    rename_track(&mut app, id, "");
    assert_eq!(name(&app).as_deref(), Some(""), "空で確定すると未命名に戻る");
    rename_track(&mut app, id, "5");
    assert_eq!(name(&app).as_deref(), Some("5"), "位置と違う数字はユーザーが付けた名前");

    app.cur.song_doc.mark_saved();
    rename_track(&mut app, id, "5");
    assert!(!app.cur.song_doc.is_dirty(), "同じ名前の確定は編集を積まない");

    app.handle_event(AppEvent::Launcher(LauncherEvent::AddScene));
    app.handle_event(AppEvent::Launcher(LauncherEvent::AddScene));
    let scene = app.cur.song_doc.song().scenes[1].id;
    let scene_name = |app: &AppData| app.cur.song_doc.song().scenes[1].name.clone();
    app.handle_event(AppEvent::Launcher(LauncherEvent::BeginRenameScene(scene)));
    assert_eq!(app.cur.launcher.scene_rename_text, "2", "列の初期値も番号");
    app.handle_event(AppEvent::Launcher(LauncherEvent::CancelRenameScene));
    rename_scene(&mut app, scene, "サビ");
    assert_eq!(scene_name(&app), "サビ");
    rename_scene(&mut app, scene, "2");
    assert_eq!(scene_name(&app), "", "名前付きの列でも番号を確定すると未命名に戻る");
}

#[test]
fn 未命名トラックの複製は未命名のまま新しい位置の番号になり名前付きは名前を写す() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let (lead, plain) = (track_ids(&app)[0], track_ids(&app)[1]);
    rename_track(&mut app, lead, "Lead");

    let before: HashSet<u32> = track_ids(&app).into_iter().collect();
    app.handle_event(AppEvent::DuplicateTracksUnique(vec![plain]));
    app.handle_event(AppEvent::DuplicateTracksShared(vec![lead]));

    let song = app.cur.song_doc.song();
    let added: Vec<(usize, &common::model::Track)> =
        song.tracks.iter().enumerate().filter(|(_, t)| !before.contains(&t.id)).collect();
    let names: Vec<&str> = added.iter().map(|(_, t)| t.name.as_str()).collect();
    assert_eq!(names.len(), 2, "複製が 2 本");
    assert!(names.contains(&"Lead"), "名前付きは名前ごと写す: {names:?}");
    let (pos, copy) = added.iter().find(|(_, t)| t.name.is_empty()).copied().expect("未命名の複製");
    assert_eq!(song.track_display_name(copy.id), (pos + 1).to_string(), "複製は新しい位置の番号で出る");
}

/// プラグイン窓のタイトルは開いたときに 1 度作るだけだと、未命名トラックの番号が上にトラックを
/// 足すだけで変わるので、窓だけ古い番号を出し続ける。frame 末の追従で差分だけ送り直す。
/// (エディタ窓を開く経路は Windows だけ。)
#[cfg(windows)]
#[test]
fn プラグイン窓のタイトルは開いた後の番号の変化と改名に追従する() {
    let (mut app, _a, mut plugin_rx, _d) = support::build_app();
    support::load_instrument(&mut app);
    let track_id = app.cur.song_doc.song().tracks[0].id;
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("楽器").id;
    support::drain(&mut plugin_rx);

    app.handle_event(AppEvent::Device(DeviceEvent::ToggleSlotGui { device_id }));
    let opened: Vec<String> = support::drain(&mut plugin_rx)
        .into_iter()
        .filter_map(|c| match c {
            PluginCommand::OpenSlotGuiEmbedded { title, .. } => Some(title),
            _ => None,
        })
        .collect();
    assert_eq!(opened, ["Plugin — 1 / Test Synth [Untitled]"]);

    let mut resent_titles = |app: &mut AppData| -> Vec<String> {
        app.sync_all_plugin_editor_titles();
        support::drain(&mut plugin_rx)
            .into_iter()
            .filter_map(|c| match c {
                PluginCommand::SetSlotGuiTitle { title, .. } => Some(title),
                _ => None,
            })
            .collect()
    };
    assert!(resent_titles(&mut app).is_empty(), "何も変わっていなければ送らない");

    select_track_single(&mut app, 0);
    app.handle_event(AppEvent::AddInstrumentTrack); // 選択中の行の直上に入る
    assert_eq!(resent_titles(&mut app), ["Plugin — 2 / Test Synth [Untitled]"], "上に 1 本足すと 2");
    assert!(resent_titles(&mut app).is_empty(), "送るのは変わったときだけ");

    rename_track(&mut app, track_id, "Lead");
    assert_eq!(resent_titles(&mut app), ["Plugin — Lead / Test Synth [Untitled]"]);
}

#[test]
fn 旧既定名のトラックは開いてもそのまま残り未保存マークも付かない() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj.daw");
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        song.tracks[0].name = "Track 1".into();
        let mut third = song.tracks[0].clone();
        third.id = song.alloc_track_id();
        third.name = "Track 3".into();
        song.tracks.push(third);
        let mut unnamed = song.tracks[0].clone();
        unnamed.id = song.alloc_track_id();
        unnamed.name = String::new();
        song.tracks.push(unnamed);
    });
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    app.cur.song_doc.mark_saved();

    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    assert_eq!(app.cur.song_doc.file_path.as_ref(), Some(&proj));
    assert_eq!(track_names(&app), ["Track 1", "Track 3", ""], "ユーザーの名前と区別できないので書き換えない");
    assert_eq!(mixer_labels(&app), ["Track 1", "Track 3", "3"]);
    assert!(!app.cur.song_doc.is_dirty(), "開いただけで * を付けない");
}
