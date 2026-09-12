//! `docs/plan_project_tabs.md` §5.7: プロジェクトタブの状態機械 (コマンド / イベント層)。
//!
//! タブの生死とガード (New / Open / Close / Quit の確認順) は `dirty_guard.rs` が持つ。
//! ここはそれ以外 — 切替 / 巡回 / 並べ替え / 編集の独立 / 背景タブ宛 IPC event の配送 /
//! engine への project 宛 command / 上限 / ドラッグ中の Ctrl+Tab 保留。

use common::audio_bridge::MAX_PROJECTS;
use common::protocol::{AudioCommand, DeviceAddr, PluginCommand, PluginEvent, ProjectKey};

use daw_gui::app::{AppEvent, DirtyGuardAction};
use daw_gui::event_tabs::TabEvent;
use daw_gui::shutdown::QuitRequest;

use super::support::{self, drain, load_instrument};

#[test]
fn startup_opens_the_first_project_and_scopes_it() {
    let (app, mut audio_rx, _plugin_rx, _d) = support::build_app();
    let sent = drain(&mut audio_rx);
    let first = app.pk();
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::OpenProject { project } if *project == first)),
        "engine slot for the first tab: {sent:?}"
    );
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::SetScopeProject { project } if *project == first)),
        "scope (meters / sampler master) follows the first tab: {sent:?}"
    );
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs.order, vec![first]);
}

#[test]
fn new_tab_opens_engine_slot_and_switch_sends_scope() {
    let (mut app, mut audio_rx, _plugin_rx, _d) = support::build_app();
    let a = app.pk();
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    assert_ne!(a, b);
    let sent = drain(&mut audio_rx);
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::OpenProject { project } if *project == b)),
        "OpenProject for the new tab: {sent:?}"
    );
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::SetScopeProject { project } if *project == b)),
        "scope moves to the new tab: {sent:?}"
    );

    app.handle_event(AppEvent::Tab(TabEvent::Switch(a)));
    assert_eq!(app.pk(), a);
    let sent = drain(&mut audio_rx);
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::SetScopeProject { project } if *project == a)),
        "scope follows the switch: {sent:?}"
    );
    assert!(
        !sent.iter().any(|c| matches!(c, AudioCommand::OpenProject { .. })),
        "switching does not re-open anything: {sent:?}"
    );
}

#[test]
fn next_prev_cycle_through_the_display_order() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let c = app.pk();
    assert_eq!(app.tabs.order, vec![a, b, c]);

    app.handle_event(AppEvent::Tab(TabEvent::Next));
    assert_eq!(app.pk(), a, "wraps from the last to the first");
    app.handle_event(AppEvent::Tab(TabEvent::Next));
    assert_eq!(app.pk(), b);
    app.handle_event(AppEvent::Tab(TabEvent::Prev));
    assert_eq!(app.pk(), a);
    app.handle_event(AppEvent::Tab(TabEvent::Prev));
    assert_eq!(app.pk(), c, "wraps from the first to the last");
}

#[test]
fn move_reorders_without_changing_the_active_tab() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let c = app.pk();

    app.handle_event(AppEvent::Tab(TabEvent::Move { key: c, to: 0 }));
    assert_eq!(app.tabs.order, vec![c, a, b]);
    assert_eq!(app.pk(), c, "active tab unchanged");
    app.handle_event(AppEvent::Tab(TabEvent::Move { key: a, to: 5 }));
    assert_eq!(app.tabs.order, vec![c, b, a], "clamped to the end");
    app.handle_event(AppEvent::Tab(TabEvent::Next));
    assert_eq!(app.pk(), b, "Next follows the new order");
}

#[test]
fn edits_and_undo_stay_inside_their_tab() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.edit_song(|song| song.bpm = 99.0);
    assert!(app.cur.song_doc.is_dirty());

    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    assert!(!app.cur.song_doc.is_dirty(), "the new tab starts clean");
    assert!(!app.cur.song_doc.can_undo(), "the new tab has no history");
    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.song_doc.song().bpm, 120.0, "undo on B does nothing to B");
    assert_eq!(app.tab(a).unwrap().song_doc.song().bpm, 99.0, "undo on B does not touch A");

    app.handle_event(AppEvent::Tab(TabEvent::Switch(a)));
    app.handle_event(AppEvent::Undo);
    assert_eq!(app.cur.song_doc.song().bpm, 120.0, "A's own undo reverts A");
    assert_eq!(app.tab(b).unwrap().song_doc.song().bpm, 120.0);
}

#[test]
fn plugin_events_for_a_background_tab_land_in_that_tab() {
    let (mut app, _a, mut plugin_rx, _d) = support::build_app();
    let a = app.pk();
    // A に synth を積む (SetSlotPlugin は A の住所で出る)。
    let track_id = app.cur.song_doc.song().tracks[0].id;
    support::select_track_single(&mut app, 0);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: false,
    });
    let device_id = daw_gui::app::device_id_at(app.cur.song_doc.song(), track_id, 0).unwrap();
    let generation = app.cur.pipc.pending_plugin_loads[&device_id];
    let sent = drain(&mut plugin_rx);
    assert!(
        sent.iter().any(|m| matches!(
            m,
            PluginCommand::SetSlotPlugin { device, .. } if *device == DeviceAddr::new(a, device_id)
        )),
        "SetSlotPlugin carries tab A's address: {sent:?}"
    );

    // B をアクティブにしてから、A 宛の load 完了が届く。
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoaded {
        device: DeviceAddr::new(a, device_id),
        token: common::protocol::InstanceToken(7),
        id: "test.synth".into(),
        name: "test.synth".into(),
        shmem_id: String::new(),
        state_load_error: None,
        aux_output_count: 0,
        aux_input_count: 0,
        generation,
    }));
    assert_eq!(app.pk(), b, "delivery does not switch the active tab");
    assert!(app.cur.pipc.loaded_devices.is_empty(), "B's ledger untouched");
    let a_state = app.tab(a).unwrap();
    assert!(a_state.pipc.loaded_devices.contains_key(&device_id), "A's ledger got the device");
    assert!(a_state.pipc.pending_plugin_loads.is_empty(), "A's pending load resolved");

    // 閉じたタブ宛 (もう居ない key) は捨てる (panic せず、誰にも入らない)。
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginUnloaded {
        device: DeviceAddr::new(ProjectKey(999), device_id),
    }));
    assert!(app.tab(a).unwrap().pipc.loaded_devices.contains_key(&device_id));
}

#[test]
fn closing_a_tab_unloads_its_project_from_both_children() {
    let (mut app, mut audio_rx, mut plugin_rx, _d) = support::build_app();
    load_instrument(&mut app);
    app.cur.song_doc.mark_saved(); // clean → 確認なしで閉じる
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);

    app.handle_event(AppEvent::Tab(TabEvent::Close(a)));

    assert_eq!(app.tabs.order, vec![b]);
    assert_eq!(app.pk(), b);
    let audio = drain(&mut audio_rx);
    assert!(
        audio.iter().any(|c| matches!(c, AudioCommand::CloseProject { project } if *project == a)),
        "engine slot released: {audio:?}"
    );
    assert!(
        audio.iter().any(|c| matches!(c, AudioCommand::ClosePluginShmem { project, .. } if *project == a)),
        "A's plugin shmem closed before unload: {audio:?}"
    );
    let plugin = drain(&mut plugin_rx);
    assert!(
        plugin.iter().any(|m| matches!(m, PluginCommand::UnloadProject { project } if *project == a)),
        "host drops A's instances: {plugin:?}"
    );
    assert!(app.ui_ephemeral.retained_state_to_drop.contains(&a), "widget state scheduled for drop");
}

#[test]
fn closing_the_active_tab_moves_to_the_right_neighbour() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let c = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Switch(b)));

    app.handle_event(AppEvent::Tab(TabEvent::Close(b)));
    assert_eq!(app.tabs.order, vec![a, c]);
    assert_eq!(app.pk(), c, "the tab to the right becomes active");

    app.handle_event(AppEvent::Tab(TabEvent::Close(c)));
    assert_eq!(app.pk(), a, "wraps to the first");
}

#[test]
fn tab_limit_refuses_the_33rd_tab() {
    let (mut app, _a, _p, _d) = support::build_app();
    for _ in 1..MAX_PROJECTS {
        app.handle_event(AppEvent::Tab(TabEvent::New));
    }
    assert_eq!(app.tabs.len(), MAX_PROJECTS);
    let last = app.pk();
    app.ui_ephemeral.status_message.clear();

    app.handle_event(AppEvent::Tab(TabEvent::New));

    assert_eq!(app.tabs.len(), MAX_PROJECTS, "no 33rd tab");
    assert_eq!(app.pk(), last, "active tab unchanged");
    assert!(!app.ui_ephemeral.status_message.is_empty(), "the refusal is explained");
}

#[test]
fn flush_all_song_sync_sends_each_tab_under_its_own_key() {
    let (mut app, mut audio_rx, _p, _d) = support::build_app();
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    let _ = drain(&mut audio_rx);

    app.flush_all_song_sync();

    let sent = drain(&mut audio_rx);
    for key in [a, b] {
        assert!(
            sent.iter().any(|c| matches!(c, AudioCommand::LoadSong { project, .. } if *project == key)),
            "LoadSong for {key:?}: {sent:?}"
        );
    }
    // 変化が無ければ次は何も送らない (タブごとの epoch)。
    app.flush_all_song_sync();
    assert!(drain(&mut audio_rx).is_empty(), "no re-send without edits");
}

#[test]
fn quit_asks_each_dirty_tab_in_display_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.cur.song_doc.file_path = Some(dir.path().join("a.daw"));
    app.cur.song_doc.normalize(|_| {}); // A: dirty
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.cur.song_doc.normalize(|_| {}); // B: dirty
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let c = app.pk(); // C: clean, active

    app.request_close();

    assert_eq!(app.pk(), a, "first dirty tab is brought forward");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));
    assert!(!app.shutdown.is_shutting_down());

    // A: 保存して終了 → 次の未保存 (B) を聞く。
    app.handle_event(AppEvent::DirtyGuardSave);
    assert!(dir.path().join("a.daw").exists(), "A saved");
    assert_eq!(app.pk(), b, "then B is brought forward");
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));
    assert!(!app.shutdown.is_shutting_down());

    // B: 保存せず終了 → B の変更は捨てられ、C は clean なので終了へ。
    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert!(app.shutdown.is_shutting_down(), "all tabs resolved → shutdown");
    assert!(app.tabs.order.contains(&b), "終了時はタブを畳まない (捨てるのは変更だけ)");
    assert!(!app.tab(b).unwrap().song_doc.is_dirty(), "discarded tab is no longer asked about");
    assert!(app.tabs.order.contains(&c));
}

/// 「保存せず終了」 でタブを **閉じて** 次へ進む実装だと、書き出し中のタブは閉じるのを
/// 拒否されて dirty が残り、`continue_quit` が同じタブを永久に聞き続ける (答えても終われない)。
/// 捨てると決めたタブは dirty を落とすだけにして、この無限ループを構造的に消す。
#[test]
fn quit_discard_on_an_exporting_tab_does_not_loop() {
    let (mut app, _a, _p, _d) = support::build_app();
    let busy = app.pk();
    app.cur.song_doc.normalize(|_| {});
    // 書き出し中 (= 閉じるのを拒否されるタブ)。
    app.cur.transport.export_stage = Some(daw_gui::app::ExportStage::AudioRender { done: 0, total: 1 });

    app.request_close();
    assert_eq!(app.ui_ephemeral.dirty_guard, Some(DirtyGuardAction::Quit(QuitRequest::USER)));

    app.handle_event(AppEvent::DirtyGuardDiscard);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "同じタブをもう一度聞かない");
    assert!(app.shutdown.is_shutting_down(), "終了まで進む");
    assert!(!app.tab(busy).unwrap().song_doc.is_dirty());
}

#[test]
fn quit_cancel_on_the_second_tab_keeps_everything() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.cur.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.cur.song_doc.normalize(|_| {});

    app.request_close();
    assert_eq!(app.pk(), a);
    app.handle_event(AppEvent::DirtyGuardDiscard); // A の変更は捨てる
    assert_eq!(app.pk(), b);
    app.handle_event(AppEvent::DirtyGuardCancel);

    assert!(!app.shutdown.is_shutting_down(), "cancel stops the quit");
    assert_eq!(app.tabs.order, vec![a, b], "終了を取り消してもタブは畳まれない");
    assert!(!app.tab(a).unwrap().song_doc.is_dirty(), "A の変更は捨てられた");
    assert!(app.cur.song_doc.is_dirty(), "B keeps its edits");
}

#[test]
fn ctrl_tab_during_a_clip_drag_is_deferred_to_the_arrangement() {
    let (mut app, _a, _p, _d) = support::build_app();
    let a = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.handle_event(AppEvent::Tab(TabEvent::Switch(a)));

    // アレンジ widget が「クリップ Move / トラック並べ替え中」を写している間は、
    // 切替を保留して widget に payload へ昇格させる (§5.6)。
    app.cur.peph.arrange_xfer_drag_active = true;
    app.handle_event(AppEvent::Tab(TabEvent::Next));
    assert_eq!(app.pk(), a, "not switched yet");
    assert_eq!(app.ui_ephemeral.pending_tab_switch, Some(b), "switch deferred to the widget");

    // ドラッグしていなければ即切替。
    app.ui_ephemeral.pending_tab_switch = None;
    app.cur.peph.arrange_xfer_drag_active = false;
    app.handle_event(AppEvent::Tab(TabEvent::Next));
    assert_eq!(app.pk(), b);
    assert_eq!(app.ui_ephemeral.pending_tab_switch, None);
}

#[test]
fn dropped_clips_from_another_tab_become_independent_copies() {
    let (mut app, _a, _p, _d) = support::build_app();
    // A: クリップ 1 個。
    let track_id = app.cur.song_doc.song().tracks[0].id;
    // `CreateClip.track` は song.tracks の index。
    app.handle_event(AppEvent::CreateClip { track: 0, start_beat: 4.0 });
    let a_clip = app.cur.song_doc.song().tracks[0].clips[0].clone();
    let refs = vec![common::model::ClipKey { track_id, clip_id: a_clip.id }];
    let (envelope, base, min_track) = app.clips_copy_envelope(&refs).expect("envelope");
    assert_eq!((base, min_track), (4.0, 0));

    // B へ落とす (cross-project paste と同じ口)。
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b_track = app.cur.song_doc.song().tracks[0].id;
    let clips = match envelope.payload {
        daw_gui::clipboard::ClipboardPayload::Clips(c) => c,
        _ => unreachable!(),
    };
    let n = app.paste_clips_at(clips, envelope.source_project_id, b_track, 8.0, &envelope.media);
    assert_eq!(n, 1);
    let b_clip = &app.cur.song_doc.song().tracks[0].clips[0];
    assert_eq!(b_clip.start_beat, 8.0, "placed at the drop beat");
    // content は B の Song に **独立して** 採番される (id 空間はタブごとなので数値の
    // 一致 / 不一致に意味は無い。B の Song が自分の entry を持つことが独立の証拠)。
    let b_content = b_clip.content_id;
    assert!(app.cur.song_doc.song().clip_contents.contains_key(&b_content));
    assert_eq!(app.cur.song_doc.song().clip_contents.len(), 1, "B got exactly one new content");
    assert!(app.cur.song_doc.is_dirty(), "B is edited");
    // A は無変更。
    app.handle_event(AppEvent::Tab(TabEvent::Prev));
    assert_eq!(app.cur.song_doc.song().tracks[0].clips.len(), 1);
    assert_eq!(app.cur.song_doc.song().tracks[0].clips[0].start_beat, 4.0);
    assert_eq!(app.cur.song_doc.song().clip_contents.len(), 1, "A's contents untouched");
}

/// 実機で出た穴の回帰: 別のタブへ運んだ **オーディオクリップが殻だけ**になっていた。
/// `ClipContent` は音源を id (Song スコープの名前) で指すので、写しが `Song.media` を
/// 連れて行かないと貼り先で `source_id` が宙に浮く (`docs/plan_project_tabs.md` §8)。
#[test]
fn audio_clips_carry_their_media_to_another_tab() {
    use common::model::{AudioContent, AudioEvent, AudioSource, AudioSourcePath, Clip, ClipContent};

    let (mut app, _a, _p, _d) = support::build_app();
    let track_id = app.cur.song_doc.song().tracks[0].id;
    app.edit_song(|song| {
        let src = song.alloc_audio_source_id();
        song.media.audio_sources.insert(
            src,
            AudioSource {
                path: AudioSourcePath::Generated { id: 7 },
                sample_rate: 48_000,
                channels: 2,
                frames: 8 * 24_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let cid = song.alloc_content(
            ClipContent::Audio(AudioContent {
                events: vec![AudioEvent {
                    id: 1,
                    source_id: src,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    source_start_frames: 0,
                    source_end_frames: 8 * 24_000,
                    ..AudioEvent::default()
                }],
                next_event_id: 2,
            }),
            "audio".to_string(),
        );
        song.tracks[0].clips = vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            ..Default::default()
        }];
    });

    let refs = vec![common::model::ClipKey { track_id, clip_id: 1 }];
    let (envelope, _, _) = app.clips_copy_envelope(&refs).expect("envelope");
    assert_eq!(envelope.media.audio.len(), 1, "写しが音源テーブルを連れている");

    // B タブへ落とす (タブ間 D&D の着地と同じ口)。
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b_track = app.cur.song_doc.song().tracks[0].id;
    let clips = match envelope.payload {
        daw_gui::clipboard::ClipboardPayload::Clips(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(app.paste_clips_at(clips, envelope.source_project_id, b_track, 0.0, &envelope.media), 1);

    let song = app.cur.song_doc.song();
    let clip = &song.tracks[0].clips[0];
    let ClipContent::Audio(audio) = song.clip_contents.get(&clip.content_id).expect("content")
    else {
        panic!("オーディオの content が貼られていない");
    };
    let src = audio.events[0].source_id;
    let entry = song.media.audio_sources.get(&src).expect("音源が B のテーブルに取り込まれる");
    assert_eq!(entry.path, AudioSourcePath::Generated { id: 7 });
    assert_eq!(entry.frames, 8 * 24_000);
}

/// レビューで出た穴の回帰: 持ち込みは常に独立コピー (`source_project_id = 0`) なので、
/// **元のタブへ戻して落としても媒体の取り込みが走る**。写しは絶対パスなので、貼り先の
/// フォルダ基準へ戻してから取り込まないと、同じ音源が `ProjectRelative` と `Absolute` の
/// 2 本になる (`Song::import_media` はパスの一致で流用を決める)。
#[test]
fn dropping_back_into_the_source_tab_reuses_the_same_media_entry() {
    use common::model::{AudioContent, AudioEvent, AudioSource, AudioSourcePath, Clip, ClipContent};

    let (mut app, _a, _p, _d) = support::build_app();
    // プロジェクトのフォルダ基準を持たせる (samples/ は project_dir からの相対)。
    app.cur.song_doc.file_path = Some(std::path::PathBuf::from("C:/songs/demo.daw"));
    let track_id = app.cur.song_doc.song().tracks[0].id;
    app.edit_song(|song| {
        let src = song.alloc_audio_source_id();
        song.media.audio_sources.insert(
            src,
            AudioSource {
                path: AudioSourcePath::ProjectRelative(std::path::PathBuf::from("samples/a.wav")),
                sample_rate: 48_000,
                channels: 2,
                frames: 24_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let cid = song.alloc_content(
            ClipContent::Audio(AudioContent {
                events: vec![AudioEvent {
                    id: 1,
                    source_id: src,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 1.0,
                    source_start_frames: 0,
                    source_end_frames: 24_000,
                    ..AudioEvent::default()
                }],
                next_event_id: 2,
            }),
            "audio".to_string(),
        );
        song.tracks[0].clips = vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 1.0,
            content_id: cid,
            ..Default::default()
        }];
    });

    // 持ち込みの写し (絶対パスに解かれる) を、同じタブへ落とす。
    let refs = vec![common::model::ClipKey { track_id, clip_id: 1 }];
    let (mut envelope, _, _) = app.clips_copy_envelope(&refs).expect("envelope");
    envelope.source_project_id = 0; // §5.6: 持ち込みは常に独立コピー
    assert!(
        matches!(
            envelope.media.audio[0].1.path,
            AudioSourcePath::Absolute(_)
        ),
        "写しは絶対パスで運ぶ"
    );
    let clips = match envelope.payload {
        daw_gui::clipboard::ClipboardPayload::Clips(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(app.paste_clips_at(clips, 0, track_id, 4.0, &envelope.media), 1);

    let song = app.cur.song_doc.song();
    assert_eq!(song.media.audio_sources.len(), 1, "音源は 1 本のまま (二重登録しない)");
    assert_eq!(song.tracks[0].clips.len(), 2);
}
