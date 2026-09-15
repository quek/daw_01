//! plugin の読み込み中に押したオフライン描画 (書き出し / 解析 / Bounce) は、断らずに読み込みの確定 (成功 / 失敗) を
//! 待ってから始まる (再生の A7 と同じ規則、`handler/render_wait.rs`)。
//!
//! 読み込み中の plugin を鳴らすトラックは engine のグラフに入らない (r.md #131) ので、待たずに焼くとそのトラックは
//! 無音で焼かれる。待っている間に何を止め何を通すか (再生 / ほかの描画 / キャンセル) と、読み込みが「応答以外」
//! (読み込み中の plugin を消す undo) で空になっても始まることをここで留める。

use common::model::{Clip, ClipContent, ClipKey, MidiContent, Note};
use common::protocol::{AudioCommand, PluginCommand, PluginEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent, ExportStage, FileDialogKind};
use daw_gui::state::{LoudnessPhase, PendingRender};

use super::support::{build_app, drain, fake_plugin_loaded, select_track_single};

/// track 0 に picker で `plugin_id` を足す (応答はまだ返さない = 読み込み中)。`(track_id, device_id)`。
fn loading_plugin(app: &mut AppData, plugin_id: &str) -> (u32, u64) {
    let track_id = app.cur.song_doc.song().tracks[0].id;
    select_track_single(app, 0);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::SelectPluginFromDb { id: plugin_id.into(), keep_open: false, open_gui: false });
    let device_id = app.cur.song_doc.song().tracks[0].plugins().last().expect("足した plugin").id;
    assert!(app.cur.pipc.pending_plugin_loads.contains_key(&device_id), "前提: 読み込み中");
    (track_id, device_id)
}

fn export_wav(app: &mut AppData) {
    app.handle_event(AppEvent::FileDialogResult {
        kind: FileDialogKind::ExportWav { range: None },
        paths: vec![std::path::PathBuf::from("C:/out.wav")],
    });
}

fn reinits(rx: &mut UnboundedReceiver<PluginCommand>) -> usize {
    drain(rx).iter().filter(|m| matches!(m, PluginCommand::ReinitAllPlugins { .. })).count()
}

fn position(msgs: &[AudioCommand], pred: impl Fn(&AudioCommand) -> bool) -> Option<usize> {
    msgs.iter().position(pred)
}

/// 書き出しは読み込み中に押しても断らず、確定を待ってから始まる — 失敗で確定しても始まる (失敗した device は素通しで
/// 鳴るのが規則なので、待つ理由が無くなる)。待っている間は書き出しが始まっている扱い: 再生を止め、再生もほかの描画も
/// 始めない。
#[test]
fn 読み込み中の書き出しは確定を待って始まり_待つ間は再生もほかの描画も始めない() {
    let (mut app, mut audio_rx, mut plugin_rx, _d) = build_app();
    let (_track, device_id) = loading_plugin(&mut app, "test.synth");
    app.cur.transport.is_playing = true;
    drain(&mut audio_rx);
    drain(&mut plugin_rx);

    export_wav(&mut app);
    assert!(matches!(app.cur.transport.pending_render, Some(PendingRender::Wav { .. })), "書き出しを預かる");
    assert!(app.cur.transport.export_stage.is_none(), "読み込み中に書き出しを始めた");
    assert_eq!(reinits(&mut plugin_rx), 0, "読み込み中に plugin の再初期化を始めた");
    assert!(drain(&mut audio_rx).iter().any(|m| matches!(m, AudioCommand::Stop { .. })), "押した時点で再生を止める");
    app.cur.transport.is_playing = false;

    app.handle_event(AppEvent::Play);
    assert!(!drain(&mut audio_rx).iter().any(|m| matches!(m, AudioCommand::Play { .. })), "待っている間に再生した");
    assert_eq!(app.cur.transport.pending_play, None, "読み込み後に再生を予約した");
    app.handle_event(AppEvent::AnalyzeLoudness);
    assert!(app.ui_ephemeral.export_range_picker.is_none(), "待っている間にほかの描画を始めた");
    assert!(app.ui_ephemeral.status_message.contains("WAV 書き出しの開始待ち"), "{}", app.ui_ephemeral.status_message);

    let generation = app.cur.pipc.pending_plugin_loads[&device_id];
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoadFailed {
        device: app.dev(device_id),
        plugin_id: "test.synth".into(),
        reason: "boom".into(),
        generation,
    }));
    assert!(app.cur.transport.pending_render.is_none());
    assert!(matches!(app.cur.transport.export_stage, Some(ExportStage::AudioRender { .. })), "確定したら書き出しを始める");
    assert_eq!(reinits(&mut plugin_rx), 1, "書き出しのクリーンスタート (再初期化) から始まる");
}

/// 待っている書き出し / 解析は取り消せる。取り消した後に読み込みが確定しても始まらない。取り消さなければ確定で始まる。
#[test]
fn 読み込み待ちの書き出しと解析は取り消すと始まらず_取り消さなければ確定で始まる() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let (track_id, _device_id) = loading_plugin(&mut app, "test.synth");

    export_wav(&mut app);
    app.handle_event(AppEvent::CancelExport);
    assert!(app.cur.transport.pending_render.is_none());
    assert_eq!(app.ui_ephemeral.status_message, "WAV 書き出しをキャンセルしました");

    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    assert!(app.loudness_in_progress(), "解析を預かっている間はレポート窓が解析中として塞ぐ");
    assert!(app.ui_prefs.loudness_report_open, "待っている間の表示はレポート窓");
    assert_eq!(app.cur.loudness.phase, LoudnessPhase::Idle, "読み込み中に解析を始めた");
    app.handle_event(AppEvent::CancelLoudnessAnalysis);
    assert!(!app.loudness_in_progress());
    drain(&mut plugin_rx);

    // もう一度頼んでから確定させる。取り消した書き出しは始まらず、頼み直した解析だけが始まる。
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth");
    assert!(app.cur.transport.export_stage.is_none(), "取り消した書き出しが始まった");
    assert!(matches!(app.cur.loudness.phase, LoudnessPhase::AwaitingReinit { .. }), "確定したら解析を始める");
    assert_eq!(reinits(&mut plugin_rx), 1);
}

/// Bounce は走っている間も画面を塞がないので、待っている間も再生を頼める (A7 と同じく読み込み後に走り出す)。
/// 読み込みが **応答以外** で空になっても始まる: 読み込み中の plugin を足した操作を undo すると、その event の終わりに
/// 預かった再生 → Bounce の順に出る。Bounce の完了は押した操作の名前で積む (出した event の名前ではない)。
#[test]
fn 読み込み待ちの_bounce_は再生を妨げず_読み込み中の_plugin_を消す_undo_でも始まる() {
    let (mut app, mut audio_rx, _plugin_rx, _d) = build_app();
    let dir = tempfile::tempdir().expect("tempdir");
    app.cur.song_doc.file_path = Some(dir.path().join("proj.daw"));
    let track_id = app.cur.song_doc.song().tracks[0].id;
    let clip_id = app
        .edit_song(|song| {
            let note = Note { id: 1, start_beat: 0.0, duration_beats: 1.0, pitch: 60, velocity: 100, lyric: None, muted: false };
            let content_id =
                song.alloc_content(ClipContent::Midi(MidiContent { notes: vec![note], next_note_id: 2 }), "phrase".into());
            let track = song.track_by_id_mut(track_id).expect("track");
            track.place_clip(Clip { start_beat: 0.0, length_beats: 4.0, content_id, ..Clip::default() })
        })
        .expect("clip");
    loading_plugin(&mut app, "test.fx");
    drain(&mut audio_rx);

    app.handle_event(AppEvent::BounceClipInPlace(ClipKey { track_id, clip_id }));
    assert!(matches!(app.cur.transport.pending_render, Some(PendingRender::Bounce { .. })), "Bounce を預かる");
    assert!(app.cur.pipc.pending_clip_fx_bounce.is_none(), "読み込み中に焼き始めた");
    app.handle_event(AppEvent::Play);
    assert!(app.cur.transport.pending_play.is_some(), "Bounce を待っている間も再生は頼める (読み込み後に走る)");
    assert!(!drain(&mut audio_rx).iter().any(|m| matches!(m, AudioCommand::BounceClipFxOnline { .. })));

    app.handle_event(AppEvent::Undo);
    assert!(app.cur.pipc.pending_plugin_loads.is_empty(), "前提: undo で読み込み中の plugin が消えた");
    let msgs = drain(&mut audio_rx);
    let play = position(&msgs, |m| matches!(m, AudioCommand::Play { .. })).expect("預かった再生が出る");
    let bounce = position(&msgs, |m| matches!(m, AudioCommand::BounceClipFxOnline { .. })).expect("Bounce が始まる");
    assert!(play < bounce, "預かった順に出す: {msgs:?}");
    assert!(app.cur.transport.pending_render.is_none());
    let pending = app.cur.pipc.pending_clip_fx_bounce.as_ref().expect("焼いている");
    assert_eq!(pending.label, "バウンス", "完了は Bounce を押した操作の名前で積む");
}
