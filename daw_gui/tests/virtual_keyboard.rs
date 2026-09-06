//! r.md #113: PC キーボードによる仮想鍵盤 — AppData 側 headless 回帰。
//!
//! 検証するのは「PC キー → カーソルトラック (選択中、 R 不要) への発音 / 消音」 の
//! 状態遷移と IPC。 実機でしか出ない症状 (押しっぱなし / オクターブを変えた後に止まらない /
//! 閉じても鳴り続ける) を assert で塞ぐ。

use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use daw_ui_core::GrabbedKey;
use daw_ui_platform::PhysicalKey;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_virtual_keyboard::VirtualKeyboardEvent as E;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
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
    (app, audio_rx, plugin_rx)
}

/// 発音 / 消音の IPC だけを `(on, track_id, pitch, velocity)` に畳んで取り出す。
fn drain_notes(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<(bool, u32, u8, u8)> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            AudioCommand::PreviewNoteOn { track_id, pitch, velocity } => {
                v.push((true, track_id, pitch, velocity));
            }
            AudioCommand::PreviewNoteOff { track_id, pitch } => v.push((false, track_id, pitch, 0)),
            _ => {}
        }
    }
    v
}

/// 先頭トラックをカーソル (選択) にする。 R は付けない。
fn select_first_track(app: &mut AppData) -> u32 {
    let track_id = app.song_doc.song().tracks[0].id;
    app.selection.selected_track_ids = vec![track_id];
    assert_eq!(app.virtual_keyboard_target_track(), Some(track_id));
    track_id
}

fn key(c: char, pressed: bool) -> AppEvent {
    AppEvent::VirtualKeyboard(E::Key(GrabbedKey {
        key: PhysicalKey::Char(c),
        pressed,
        repeat: false,
        shift: false,
    }))
}

fn shifted(c: char, pressed: bool) -> AppEvent {
    AppEvent::VirtualKeyboard(E::Key(GrabbedKey {
        key: PhysicalKey::Char(c),
        pressed,
        repeat: false,
        shift: true,
    }))
}

fn open(app: &mut AppData) {
    app.handle_event(AppEvent::VirtualKeyboard(E::Toggle));
    assert!(app.virtual_keyboard.open);
}

/// 下段 Z = C3 (48) が既定ベロシティで **選択中のトラック (R 不要)** に鳴り、 離すと止まる。
/// 上段 Q と下段 , は同じ C4 (60)、 上段 I は C5 (72) (ユーザー指定「I と , もド」)。
#[test]
fn 押すと選択トラックで鳴り_離すと止まる() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = select_first_track(&mut app);
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);

    app.handle_event(key('Z', true));
    app.handle_event(key('Z', false));
    app.handle_event(key('Q', true));
    app.handle_event(key('Q', false));
    app.handle_event(key(',', true));
    app.handle_event(key(',', false));
    app.handle_event(key('I', true));
    app.handle_event(key('I', false));
    assert_eq!(
        drain_notes(&mut audio_rx),
        vec![
            (true, track_id, 48, 100),
            (false, track_id, 48, 0),
            (true, track_id, 60, 100),
            (false, track_id, 60, 0),
            (true, track_id, 60, 100),
            (false, track_id, 60, 0),
            (true, track_id, 72, 100),
            (false, track_id, 72, 0),
        ]
    );
    assert!(app.virtual_keyboard.held.is_empty());
    // MIDI Capture にも MIDI デバイスと同じく常に溜まる (4 音、全部離した)。
    let captured: Vec<(u8, u8)> =
        app.midi_capture.notes.iter().map(|n| (n.pitch, n.velocity)).collect();
    assert_eq!(captured, vec![(48, 100), (60, 100), (60, 100), (72, 100)]);
    assert!(app.midi_capture.notes.iter().all(|n| n.off_ns.is_some()), "離した音は閉じている");
}

/// トラックを選択していなければ何も鳴らない (MIDI Capture には溜まる)。 音のキーでない A / K も何も起こさない。
#[test]
fn 選択トラックが無ければ鳴らない() {
    let (mut app, mut audio_rx, _p) = build_app();
    app.selection.selected_track_ids.clear();
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(key('Z', true));
    app.handle_event(key('A', true));
    app.handle_event(key('K', true));
    assert!(drain_notes(&mut audio_rx).is_empty());
    assert_eq!(app.virtual_keyboard.held.len(), 1, "押した事実は控える (選択した後の離すで矛盾しない)");
    assert_eq!(app.midi_capture.notes.len(), 1, "宛先が無くても MIDI Capture には溜まる");
}

/// 押している最中にカーソルを別トラックへ移しても、 鳴らしたトラックで止まる
/// (消音は「鳴らした台帳」 を引く)。 次に押した音は新しいカーソルトラックで鳴る。
#[test]
fn 押下中にカーソルを移しても鳴らしたトラックで止まる() {
    let (mut app, mut audio_rx, _p) = build_app();
    let first = select_first_track(&mut app);
    app.handle_event(AppEvent::AddInstrumentTrack);
    let second = app.song_doc.song().tracks.iter().map(|t| t.id).find(|id| *id != first).expect("2 本目");
    app.selection.selected_track_ids = vec![first];
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);

    app.handle_event(key('Z', true));
    app.selection.selected_track_ids = vec![second];
    app.handle_event(key('X', true));
    app.handle_event(key('Z', false));
    app.handle_event(key('X', false));
    assert_eq!(
        drain_notes(&mut audio_rx),
        vec![
            (true, first, 48, 100),
            (true, second, 50, 100),
            (false, first, 48, 0),
            (false, second, 50, 0),
        ]
    );
}

/// `[` / `]` でオクターブ、 `{` / `}` (Shift) でベロシティ ±10。 押している最中に
/// オクターブを変えても、 鳴っている音は押した時の高さで止まる。
#[test]
fn オクターブとベロシティのキー_押下中のオクターブ変更でも正しく止まる() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = select_first_track(&mut app);
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);

    app.handle_event(key(']', true));
    app.handle_event(key(']', false));
    assert_eq!(app.ui_prefs.virtual_keyboard_base_pitch, 60);
    app.handle_event(shifted('[', true));
    app.handle_event(shifted('[', false));
    assert_eq!(app.ui_prefs.virtual_keyboard_velocity, 90);

    app.handle_event(key('Z', true)); // C4 (60) @ 90
    app.handle_event(key('[', true)); // 押したままオクターブを下げる
    app.handle_event(key('[', false));
    assert_eq!(app.ui_prefs.virtual_keyboard_base_pitch, 48);
    app.handle_event(key('X', true)); // 新しい基準で D3 (50)
    app.handle_event(key('Z', false));
    app.handle_event(key('X', false));
    assert_eq!(
        drain_notes(&mut audio_rx),
        vec![
            (true, track_id, 60, 90),
            (true, track_id, 50, 90),
            (false, track_id, 60, 0),
            (false, track_id, 50, 0),
        ]
    );
}

/// OS の auto-repeat と同じキーの二重 press は無視する (連打しない)。
#[test]
fn auto_repeat_と二重押下は無視する() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = select_first_track(&mut app);
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(key('Z', true));
    app.handle_event(AppEvent::VirtualKeyboard(E::Key(GrabbedKey {
        key: PhysicalKey::Char('Z'),
        pressed: true,
        repeat: true,
        shift: false,
    })));
    app.handle_event(key('Z', true));
    app.handle_event(key('Z', false));
    assert_eq!(
        drain_notes(&mut audio_rx),
        vec![(true, track_id, 48, 100), (false, track_id, 48, 0)]
    );
}

/// 閉じる (K / ✕ / Esc) と非アクティブ化は、 PC キーもマウスも全部止める。
#[test]
fn 閉じると押している音を全部止める() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = select_first_track(&mut app);
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(key('Z', true));
    app.handle_event(key('X', true));
    app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(Some(64))));
    let _ = drain_notes(&mut audio_rx);

    app.handle_event(AppEvent::VirtualKeyboard(E::Toggle));
    assert!(!app.virtual_keyboard.open);
    let mut offs = drain_notes(&mut audio_rx);
    offs.sort_unstable();
    assert_eq!(
        offs,
        vec![(false, track_id, 48, 0), (false, track_id, 50, 0), (false, track_id, 64, 0)]
    );
    assert!(app.virtual_keyboard.held.is_empty());
    assert_eq!(app.virtual_keyboard.mouse_pitch, None);

    // 非アクティブ化 (runner の Focus(false) が呼ぶ) も同じ。
    open(&mut app);
    app.handle_event(key('Z', true));
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(AppEvent::VirtualKeyboard(E::ReleaseAll));
    assert_eq!(drain_notes(&mut audio_rx), vec![(false, track_id, 48, 0)]);
}

/// マウスの glissando: 押したまま隣の鍵へ滑ると 旧 off → 新 on。
#[test]
fn マウスの_glissando_は旧音を止めてから新音を鳴らす() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = select_first_track(&mut app);
    open(&mut app);
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(Some(48))));
    app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(Some(48))));
    app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(Some(50))));
    app.handle_event(AppEvent::VirtualKeyboard(E::MousePitch(None)));
    assert_eq!(
        drain_notes(&mut audio_rx),
        vec![
            (true, track_id, 48, 100),
            (false, track_id, 48, 0),
            (true, track_id, 50, 100),
            (false, track_id, 50, 0),
        ]
    );
}

/// 録音中、 カーソルトラックが録音待機 (R) なら MIDI 入力と同じ経路でクリップに書き込まれる。
#[test]
fn 録音中はカーソルトラックが_r_ならクリップに書き込まれる() {
    let (mut app, _a, _p) = build_app();
    let track_id = select_first_track(&mut app);
    app.handle_event(AppEvent::ToggleTrackArmed(track_id));
    open(&mut app);
    app.handle_event(AppEvent::ToggleMidiRecording);
    app.handle_event(AppEvent::Tick { samples: 0, preroll: 0, playing: true, recording_live: true });
    app.handle_event(key('Q', true));
    // 120 BPM / 48kHz で 1 拍 = 24000 サンプル。
    app.handle_event(AppEvent::Tick { samples: 24_000, preroll: 0, playing: true, recording_live: true });
    app.handle_event(key('Q', false));
    let song = app.song_doc.song();
    let clip = &song.tracks[0].clips[0];
    let notes = match song.clip_contents.get(&clip.content_id).expect("content") {
        common::model::ClipContent::Midi(m) => &m.notes,
        other => panic!("MIDI クリップのはず: {other:?}"),
    };
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].pitch, 60);
    assert!((notes[0].duration_beats - 1.0).abs() < 1e-6, "{}", notes[0].duration_beats);
}

/// 録音中でもカーソルトラックが録音待機でなければ書き込まない (鳴るだけ)。 録音自体は
/// 従来どおり R のトラックが要る。
#[test]
fn 録音中でもカーソルトラックが_r_でなければ書き込まない() {
    let (mut app, mut audio_rx, _p) = build_app();
    let first = select_first_track(&mut app);
    app.handle_event(AppEvent::AddInstrumentTrack);
    let second = app.song_doc.song().tracks.iter().map(|t| t.id).find(|id| *id != first).expect("2 本目");
    // R は 2 本目、 カーソルは 1 本目。
    app.handle_event(AppEvent::ToggleTrackArmed(second));
    app.selection.selected_track_ids = vec![first];
    open(&mut app);
    app.handle_event(AppEvent::ToggleMidiRecording);
    app.handle_event(AppEvent::Tick { samples: 0, preroll: 0, playing: true, recording_live: true });
    let _ = drain_notes(&mut audio_rx);
    app.handle_event(key('Q', true));
    app.handle_event(AppEvent::Tick { samples: 24_000, preroll: 0, playing: true, recording_live: true });
    app.handle_event(key('Q', false));
    assert_eq!(drain_notes(&mut audio_rx), vec![(true, first, 60, 100), (false, first, 60, 0)]);
    let song = app.song_doc.song();
    assert!(song.tracks.iter().all(|t| t.clips.is_empty()), "どのトラックにも書き込まない");
}
