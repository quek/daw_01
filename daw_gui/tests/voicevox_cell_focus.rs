//! r.md #87 — ランチャーのセルと VOICEVOX の噛み合わせ。
//!
//! ここで守るのは **壊れても静かに壊れる** 2 つだけ:
//!
//! - 合成順序ヒント (`SetVocalSynthPriority`) が **合成タイムラインの座標**で送られること。
//!   song の playhead に戻ると、セルはアレンジの終端より後ろの仮想区間に置かれている
//!   ぶん常に最遠 = 最後に回され、「セルの ▶ を押しても当分歌わない」になる。待てば
//!   いつかは鳴るので、ログにも `*` にも出ない。
//! - 口パクの生成物が入力を失ったら消えること。残ると **歌が無いのに口だけ動く**。
//!   再生成の経路 (`mark_lipsync_dirty`) は binding を持つ track が居なければ何も
//!   しないので、一度取り残されると二度と片付かない。

use std::sync::Arc;

use common::model::{Clip, ClipContent, ImageContent, MidiContent, MouthMap, Note, PluginInstance, SessionClip, Track};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, EditSurface, LoadedDeviceInfo};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event_launcher::LauncherRow;

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    // VOICEVOX engine の lazy spawn を封じる (テストが exe を起動しない)。
    app.voicevox.voicevox_launch_attempted = true;
    (app, plugin_rx)
}

/// 歌詞つき note 1 つを持つ MIDI content。
fn sing_content(song: &mut common::model::Song, lyric: &str) -> common::model::ContentId {
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Midi(MidiContent {
            notes: vec![Note {
                id: 1,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some(lyric.to_string()),
                muted: false,
            }],
            next_note_id: 2,
        }),
    );
    cid
}

/// 送信済み `PluginCommand` を全部取り出す。
fn drain(rx: &mut UnboundedReceiver<PluginCommand>) -> Vec<PluginCommand> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        out.push(msg);
    }
    out
}

/// 1 回の `sync_vocal_metadata` が送った `(順序ヒント, セル `clip_id` の区間原点)`。
/// **ヒントの正しさは metadata 自身と突き合わせて判定する** — 期待値を計算式で
/// 書き写すと本番の算術の写経になるので、同じ送信に載っている `cell_base_beat`
/// (= builtin が buffer 位置を解くのに使う値そのもの) を基準にする。
fn priority_and_cell_base(msgs: &[PluginCommand], cell_clip_id: u32) -> (f64, f64) {
    let hint = msgs
        .iter()
        .find_map(|m| match m {
            PluginCommand::SetVocalSynthPriority { playhead_beats, .. } => Some(*playhead_beats),
            _ => None,
        })
        .expect("順序ヒントが送られている");
    let base = msgs
        .iter()
        .find_map(|m| match m {
            PluginCommand::SetBuiltinPluginNoteMetadata { entries, .. } => entries
                .iter()
                .find(|e| e.clip_id == cell_clip_id)
                .map(|e| e.cell_base_beat),
            _ => None,
        })
        .expect("セルの note metadata が送られている");
    (hint, base)
}

/// vocal track (id 1 / device 5) + アレンジの歌 clip (id 1) + 列 0 のセル (id 2)。
fn app_with_vocal_cell() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, rx) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.scenes.clear();
        let scene_id = song.push_scene();
        let arrangement = sing_content(song, "あ");
        let cell = sing_content(song, "い");
        song.tracks.push(Track {
            id: 1,
            next_clip_id: 3,
            devices: vec![PluginInstance {
                id: 5,
                ..PluginInstance::with_ports(
                    common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                    PluginFormat::Builtin,
                    PortConfig {
                        has_note_input: true,
                        has_audio_output: true,
                        ..Default::default()
                    },
                )
            }],
            clips: vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 32.0,
                content_id: arrangement,
                ..Clip::default()
            }],
            session_clips: vec![SessionClip {
                scene_id,
                clip: Clip {
                    id: 2,
                    length_beats: 4.0,
                    content_id: cell,
                    ..Clip::default()
                },
                launch: common::model::LaunchSettings::default(),
            }],
            ..Track::default()
        });
        song.ids.next_track_id = 2;
    });
    app.ipc.loaded_devices.insert(
        5,
        LoadedDeviceInfo {
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
        },
    );
    app.transport.playhead_beat = Some(0.0);
    (app, rx)
}

/// セルを選んでいる (= セル面が直近に触った面) 間は、順序ヒントが **そのセルの
/// 仮想区間**を指す。アレンジを触っている間は playhead のまま = セルは後回し。
#[test]
fn 選んだセルが合成順序ヒントの座標を決める() {
    let (mut app, mut rx) = app_with_vocal_cell();
    let cell = app
        .cell_in_row_at_scene(LauncherRow::Track(1), app.song_doc.song().scenes[0].id)
        .expect("列 0 にセルが居る");
    app.selection.selected_launcher_cells = vec![cell];
    app.selection.last_edit_select = Some(EditSurface::LauncherCells);

    app.sync_vocal_metadata();
    let msgs = drain(&mut rx);
    // 順序ヒントは metadata より **先**に届く必要がある (合成順序は job を積んだ
    // 瞬間に 1 度だけ決まるので、後から送っても その job には効かない)。
    assert!(
        matches!(msgs.first(), Some(PluginCommand::SetVocalSynthPriority { .. })),
        "順序ヒントは note metadata より先に送る: {msgs:?}"
    );
    let (hint, base) = priority_and_cell_base(&msgs, 2);
    assert!(base > 0.0, "セルはアレンジの終端より後ろの仮想区間に置かれる");
    assert!(
        (hint - base).abs() <= 1e-9,
        "選んだセルの区間の先頭を指す: hint={hint} base={base}"
    );

    // アレンジ側へ戻る (= セル選択が降りる) → ヒントは song の playhead。
    // セル選択とアレンジの選択は排他なので、「アレンジを触った」は選択を捨てる
    // ことで表す (r.md #90 / `drop_cell_selection_if_arrangement`)。
    app.voicevox.voicevox_metadata_sent.clear();
    app.handle_event(AppEvent::ClearSelection);
    app.sync_vocal_metadata();
    let msgs = drain(&mut rx);
    let (hint, base) = priority_and_cell_base(&msgs, 2);
    assert!(
        (hint - 0.0_f64).abs() <= 1e-9 && hint < base,
        "セルを触っていなければアレンジの playhead を指す (セルの合成が割り込まない): hint={hint}"
    );
}

/// 行がセルを鳴らしていれば、選択に頼らずその座標で送る (アレンジの clip は
/// そもそも鳴らない行なので、playhead を送る意味が無い)。
#[test]
fn 鳴っている行はそのセルの座標で送る() {
    let (mut app, mut rx) = app_with_vocal_cell();
    // 行がセルを鳴らす状態 (= engine 未接続なら `Song` の起点がそのまま走行状態)。
    app.edit_song(|song| {
        song.track_by_id_mut(1).unwrap().launcher =
            common::model::RowPlayback::Launcher { clip_id: 2 };
    });
    app.selection.selected_launcher_cells.clear();
    app.selection.last_edit_select = None;

    app.sync_vocal_metadata();
    let msgs = drain(&mut rx);
    let (hint, base) = priority_and_cell_base(&msgs, 2);
    assert!(
        (hint - base).abs() <= 1e-9,
        "鳴っているセルの区間を指す: hint={hint} base={base}"
    );
}

// ---------------------------------------------------------------------------
// 口パク生成物の孤児
// ---------------------------------------------------------------------------

/// vocal track (id 1) → 口 track (id 2)。口 track には生成済みの
/// `auto_lipsync` clip と列 0 の `auto_lipsync` セルが載っている。
fn app_with_generated_lipsync(sources: u32) -> AppData {
    let (mut app, _rx) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.scenes.clear();
        let scene_id = song.push_scene();
        for i in 0..sources {
            let cid = sing_content(song, "あ");
            song.tracks.push(Track {
                id: 1 + i,
                next_clip_id: 2,
                lipsync_target_track: Some(100),
                clips: vec![Clip {
                    id: 1,
                    length_beats: 4.0,
                    content_id: cid,
                    ..Clip::default()
                }],
                ..Track::default()
            });
        }
        let img = song.alloc_content_id();
        song.clip_contents
            .insert(img, ClipContent::Image(ImageContent { events: vec![] }));
        let cell_img = song.alloc_content_id();
        song.clip_contents
            .insert(cell_img, ClipContent::Image(ImageContent { events: vec![] }));
        song.tracks.push(Track {
            id: 100,
            next_clip_id: 3,
            mouth_map: Some(MouthMap { closed: 9, ..Default::default() }),
            clips: vec![Clip {
                id: 1,
                length_beats: 4.0,
                content_id: img,
                auto_lipsync: true,
                ..Clip::default()
            }],
            session_clips: vec![SessionClip {
                scene_id,
                clip: Clip {
                    id: 2,
                    length_beats: 4.0,
                    content_id: cell_img,
                    auto_lipsync: true,
                    ..Clip::default()
                },
                launch: common::model::LaunchSettings::default(),
            }],
            launcher: common::model::RowPlayback::Launcher { clip_id: 2 },
            ..Track::default()
        });
        song.ids.next_track_id = 101;
    });
    app
}

/// 最後のソースを消したら、口 track の生成物 (アレンジの clip と列のセルの両方) が
/// 消え、そのセルを鳴らしていた行は停止に落ちる。
#[test]
fn 最後のソースを消すと口パクの生成物が残らない() {
    let mut app = app_with_generated_lipsync(1);
    app.handle_event(AppEvent::DeleteTracks(vec![1]));

    let song = app.song_doc.song();
    let mouth = song.track_by_id(100).expect("口 track は残る");
    assert!(
        mouth.clips.iter().all(|c| !c.auto_lipsync),
        "アレンジの auto_lipsync clip が残っている"
    );
    assert!(
        mouth.session_clips.iter().all(|c| !c.clip.auto_lipsync),
        "列の auto_lipsync セルが残っている (撃つと歌が無いのに口だけ動く)"
    );
    assert_eq!(
        mouth.launcher,
        common::model::RowPlayback::LauncherStopped,
        "消えたセルを指したままにしない"
    );
}

/// ソースが 1 つでも残っていれば触らない (掃除が過剰に効いて口が消えない)。
#[test]
fn ソースが残っていれば口パクの生成物は消えない() {
    let mut app = app_with_generated_lipsync(2);
    app.handle_event(AppEvent::DeleteTracks(vec![1]));

    let song = app.song_doc.song();
    let mouth = song.track_by_id(100).expect("口 track");
    assert!(mouth.clips.iter().any(|c| c.auto_lipsync));
    assert!(mouth.session_clips.iter().any(|c| c.clip.auto_lipsync));
}

/// 口 track 側を消したら、ソースに残る dangling binding を落とす
/// (残すと編集のたびに口パク再生成の debounce が空回りする)。
#[test]
fn 口トラックを消すと出力先の参照が残らない() {
    let mut app = app_with_generated_lipsync(1);
    app.handle_event(AppEvent::DeleteTracks(vec![100]));

    let song = app.song_doc.song();
    assert_eq!(song.track_by_id(1).expect("vocal").lipsync_target_track, None);
}
