//! r.md #9 の contract: **保存済みプロジェクトを開いただけでは `*` (未保存マーク) が
//! 付かない**。
//!
//! ここで守るのは「開いた後に非同期で届く子プロセスからの報告」が Song を汚さない
//! こと。plugin host は plugin を load するたびに `SlotPluginLoaded` →
//! `PluginLatencyChanged` を必ず送ってくる (`daw_plugin_host/src/main.rs`)。
//! この報告が Song の編集として扱われると、開いた直後に epoch が進み `*` が付く。

use common::model::{MASTER_TRACK_ID, PluginInstance};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::PluginEvent;

use daw_gui::app::{AppData, AppEvent};

use super::support::{self, fake_plugin_loaded, select_track_single};

/// track を 1 本足して plugin を 1 個載せ、その device_id を返す。
fn add_track_with_plugin(app: &mut AppData, plugin_id: &str) -> (u32, u64) {
    let base = app.song_doc.song().tracks[0].clone();
    let track_id = app
        .edit_song(|song| {
            let id = song.alloc_track_id();
            let mut t = base;
            t.id = id;
            t.devices.clear();
            song.tracks.push(t);
            id
        })
        .expect("edit_song");
    let idx = app.song_doc.song().tracks.len() - 1;
    select_track_single(app, idx);
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: plugin_id.into(),
        keep_open: false,
        open_gui: false,
    });
    let device_id = fake_plugin_loaded(app, track_id, 0, plugin_id);
    (track_id, device_id)
}

/// plugin host が load 直後に必ず送る latency 報告。
fn report_latency(app: &mut AppData, device_id: u64, samples: u32) {
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginLatencyChanged {
        device_id,
        samples,
    }));
}

/// 2 本のトラックに latency を報告する plugin を載せたプロジェクトを保存し、
/// 開き直す。plugin の load 応答は 1 台ずつ順に届くので、その途中経過で
/// Song が書き換わってはいけない。
#[test]
fn reopening_project_with_plugin_latency_stays_clean() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj.daw");

    let (mut app, mut audio_rx, _plugin_rx, _dispatcher) = support::build_app();
    let (_t1, d1) = add_track_with_plugin(&mut app, "test.synth");
    let (_t2, d2) = add_track_with_plugin(&mut app, "test.fx");
    report_latency(&mut app, d1, 512);
    report_latency(&mut app, d2, 256);

    common::project::save(&proj, app.song_doc.song()).expect("write project file");
    app.song_doc.mark_saved();
    assert!(!app.song_doc.is_dirty(), "保存直後は clean");

    // ---- 開き直す ----------------------------------------------------------
    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    assert_eq!(
        app.song_doc.file_path.as_ref(),
        Some(&proj),
        "clean なので確認モーダル無しで開く"
    );
    assert!(!app.song_doc.is_dirty(), "開いた直後は clean");

    // ---- plugin host からの応答が 1 台ずつ届く ------------------------------
    // device 1 だけが load 完了 + latency 報告した時点。device 2 はまだ応答が
    // 無いが、それは「device 2 の latency が 0 になった」という意味ではない。
    let t1_id = app.song_doc.song().tracks[1].id;
    let t2_id = app.song_doc.song().tracks[2].id;
    let d1 = fake_plugin_loaded(&mut app, t1_id, 0, "test.synth");
    report_latency(&mut app, d1, 512);
    assert!(
        !app.song_doc.is_dirty(),
        "1 台目の latency 報告で '*' が付いてはいけない"
    );

    let d2 = fake_plugin_loaded(&mut app, t2_id, 0, "test.fx");
    report_latency(&mut app, d2, 256);
    assert!(
        !app.song_doc.is_dirty(),
        "全 device の応答が揃っても '*' が付いてはいけない"
    );
    assert!(
        !app.song_doc.can_undo(),
        "子プロセスの報告は undo 履歴を作らない"
    );

    // dirty にしないだけでなく、PDC の入力としては engine へ届いていること
    // (= 「Song に書かない」 が「どこにも伝わらない」 になっていない)。
    let reported: Vec<(u64, u32)> = support::drain(&mut audio_rx)
        .into_iter()
        .filter_map(|cmd| match cmd {
            common::protocol::AudioCommand::SetDeviceLatency { device_id, samples } => {
                Some((device_id, samples))
            }
            _ => None,
        })
        .collect();
    assert!(
        reported.contains(&(d1, 512)) && reported.contains(&(d2, 256)),
        "報告された latency は device 単位で engine へ中継される (got {reported:?})"
    );
}

/// トラックを 1 本も持たず master fx だけを持つプロジェクト。plugin host からの
/// load 応答が Song の構造 (= 幽霊トラック "Track 1") を作ってはいけない。
#[test]
fn reopening_master_only_project_does_not_grow_a_ghost_track() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("master_only.daw");

    let (mut app, _audio_rx, _plugin_rx, _dispatcher) = support::build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.master_fx_chain.push(PluginInstance {
            id: 4001,
            ..PluginInstance::with_ports(
                "test.fx".into(),
                PluginFormat::Clap,
                PortConfig {
                    has_audio_input: true,
                    has_audio_output: true,
                    ..PortConfig::default()
                },
            )
        });
    })
    .expect("edit_song");
    common::project::save(&proj, app.song_doc.song()).expect("write project file");
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    assert!(app.song_doc.song().tracks.is_empty(), "トラック 0 本のまま開く");
    assert!(!app.song_doc.is_dirty(), "開いた直後は clean");

    fake_plugin_loaded(&mut app, MASTER_TRACK_ID, 0, "test.fx");
    assert!(
        app.song_doc.song().tracks.is_empty(),
        "子プロセスの load 応答がトラックを生やしてはいけない"
    );
    assert!(!app.song_doc.is_dirty(), "load 応答で '*' が付いてはいけない");
}

/// 保存済み device の `ports` が plugin DB の probe 結果と食い違っていても、
/// load 応答が保存値を上書きしてはいけない (port 解決の規則は
/// `PortConfig::resolve` が SSoT で「解決済みなら保つ」)。
#[test]
fn reopening_project_whose_saved_ports_differ_from_the_db_stays_clean() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("ports.daw");

    let (mut app, _audio_rx, _plugin_rx, _dispatcher) = support::build_app();
    // DB 上の "test.fx" は audio-in/out だが、保存済みの device は note 入力付き
    // (= probe 世代違い / 未 probe の環境で保存された project を模す)。
    let saved_ports = PortConfig {
        has_note_input: true,
        has_audio_output: true,
        ..PortConfig::default()
    };
    let device_index = app
        .edit_song(|song| {
            let devices = &mut song.tracks[0].devices;
            devices.push(PluginInstance {
                id: 5001,
                ..PluginInstance::with_ports("test.fx".into(), PluginFormat::Clap, saved_ports)
            });
            (devices.len() - 1) as u32
        })
        .expect("edit_song");
    common::project::save(&proj, app.song_doc.song()).expect("write project file");
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::OpenRecent(proj.clone()));
    assert!(!app.song_doc.is_dirty(), "開いた直後は clean");

    // track id は load の `ensure_ids()` で採番されるので、開いた後に読む。
    let track_id = app.song_doc.song().tracks[0].id;
    fake_plugin_loaded(&mut app, track_id, device_index, "test.fx");
    assert_eq!(
        app.song_doc.song().tracks[0].devices[device_index as usize].ports, saved_ports,
        "保存済みの port 構成は DB に上書きされない"
    );
    assert!(!app.song_doc.is_dirty(), "load 応答で '*' が付いてはいけない");
}
