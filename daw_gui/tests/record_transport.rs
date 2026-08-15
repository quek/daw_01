//! r.md #51: 録音とトランスポートの一本化 — AppData 側 headless 回帰。
//!
//! 検証するのは「Rec を押してから止めるまで」の状態遷移と IPC で、
//! いずれも実機でしか出ない症状 (プレイヘッド凍結 / Rec 点きっぱなし /
//! 止めたのに鳴り続ける) の再発を型と assert で塞ぐためのもの。
//!
//! 前提として、`transport.is_playing` と `recording.live` は **engine の観測値**
//! なので、テストは `AppEvent::Tick` を組み立てて engine の応答を模す。
//! production の poller (`daw_gui/src/main.rs::spawn_playhead_poller`) が
//! `AudioBridge` から読んで送るのと同じ内容。

use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
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

fn drain(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<AudioCommand> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

/// 先頭トラックを録音待機にする。
fn arm_first_track(app: &mut AppData) -> u32 {
    let track_id = app.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::ToggleTrackArmed(track_id));
    assert!(
        app.song_doc.song().tracks[0].armed,
        "arm_first_track: armed にならなかった"
    );
    track_id
}

/// engine からの Tick を 1 回模す (playhead は拍でなくサンプル)。
fn tick(app: &mut AppData, playing: bool, recording_live: bool, samples: u64) {
    app.handle_event(AppEvent::Tick {
        samples,
        peak_l: 0.0,
        peak_r: 0.0,
        preroll: 0,
        playing,
        recording_live,
    });
}

/// 120 BPM / 48kHz で `beat` 拍に相当するサンプル位置。
fn samples_at_beat(beat: f64) -> u64 {
    (beat * 60.0 / 120.0 * 48_000.0) as u64
}

/// 先頭トラックの先頭クリップに録音されたノート。
fn recorded_notes(app: &AppData) -> Vec<common::model::Note> {
    let song = app.song_doc.song();
    let Some(clip) = song.tracks[0].clips.first() else {
        return Vec::new();
    };
    song.clip_notes(clip).to_vec()
}

/// Rec 単独の録音開始は `play()` と同じ経路を通り、count-in → Play の順で
/// engine へ届く。旧実装は生の `Play` だけを送っていたため、GUI が「再生中」を
/// 知らずプレイヘッドが凍っていた (r.md #51 の本体)。
#[test]
fn rec_単独開始は_count_in_の後に_play_を送る() {
    let (mut app, mut audio_rx, _p) = build_app();
    arm_first_track(&mut app);
    app.handle_event(AppEvent::SetCountInBars(1));
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::ToggleMidiRecording);

    let sent = drain(&mut audio_rx);
    let start_at = sent
        .iter()
        .position(|c| matches!(c, AudioCommand::StartRecording { .. }))
        .expect("StartRecording が送られていない");
    let play_at = sent
        .iter()
        .position(|c| matches!(c, AudioCommand::Play))
        .expect("Play が送られていない");
    assert!(
        start_at < play_at,
        "count-in は Play より先に届く必要がある (逆だと 1 バッファぶん曲が進んでから count-in に入る): {sent:?}"
    );
    // 4/4 の 1 小節 = 4 拍 = 120BPM/48kHz で 96000 samples。
    assert_eq!(
        sent[start_at],
        AudioCommand::StartRecording {
            preroll_samples: 96_000
        }
    );
    assert!(app.recording.requested, "Rec ボタンは点灯する");
    assert!(
        !app.recording.live,
        "engine の観測前に録音実体が立ってはいけない (count-in を飛ばす)"
    );
}

/// count-in 無し (既定) の録音開始でも、engine に「録音中」を伝える。
/// これを送らないと engine は `recording_live` を立てず、曲末 auto-stop も
/// 抑止しない = **1 音も記録されないのに再生だけ始まる**。
#[test]
fn count_in_無しでも_start_recording_を送る() {
    let (mut app, mut audio_rx, _p) = build_app();
    arm_first_track(&mut app);
    assert_eq!(app.recording.count_in_bars, 0, "前提: 既定は count-in 無し");
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::ToggleMidiRecording);

    let sent = drain(&mut audio_rx);
    let start_at = sent
        .iter()
        .position(|c| {
            matches!(
                c,
                AudioCommand::StartRecording {
                    preroll_samples: 0
                }
            )
        })
        .unwrap_or_else(|| panic!("count-in 0 でも StartRecording を送る: {sent:?}"));
    let play_at = sent
        .iter()
        .position(|c| matches!(c, AudioCommand::Play))
        .expect("Play が送られていない");
    assert!(start_at < play_at, "録音の開始は Play より先: {sent:?}");
}

/// 録音待機のトラックが無ければ、録音も再生も始めない。
#[test]
fn 録音待機が無ければ何も始まらない() {
    let (mut app, mut audio_rx, _p) = build_app();
    for t in app.song_doc.song().tracks.iter() {
        assert!(!t.armed, "前提: 初期状態は arm されていない");
    }
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::ToggleMidiRecording);

    assert!(!app.recording.requested);
    let sent = drain(&mut audio_rx);
    assert!(
        !sent.iter().any(|c| matches!(c, AudioCommand::Play)),
        "録音先が無いのに再生だけ始めない: {sent:?}"
    );
    assert!(app.ui_ephemeral.status_message.contains("録音待機"));
}

/// 再生中の Rec (パンチイン) は count-in を使わず、停止で戻る位置も動かさない。
#[test]
fn 再生中の_rec_はパンチインで_count_in_を使わない() {
    let (mut app, mut audio_rx, _p) = build_app();
    arm_first_track(&mut app);
    app.handle_event(AppEvent::SetCountInBars(2));
    // ruler をクリックして beat 8 から再生した状態 (= 停止ホームは 8)。
    app.handle_event(AppEvent::PlayFromCursor { beat: 8.0 });
    tick(&mut app, true, false, samples_at_beat(12.0));
    let origin_before = app.transport.playback_origin_beat;
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::ToggleMidiRecording);

    let sent = drain(&mut audio_rx);
    assert_eq!(
        sent.iter()
            .filter(|c| matches!(c, AudioCommand::Play))
            .count(),
        0,
        "既に走っている transport へ Play を再送しない: {sent:?}"
    );
    assert!(
        sent.contains(&AudioCommand::StartRecording {
            preroll_samples: 0
        }),
        "パンチインは count-in 無しで録音を開始する: {sent:?}"
    );
    assert_eq!(
        app.transport.playback_origin_beat, origin_before,
        "パンチインは停止ホームを動かさない"
    );
}

/// Rec 再押下はパンチアウト — 録音だけ終わり、transport は止めない。
#[test]
fn rec_再押下は録音だけ終えて再生を続ける() {
    let (mut app, mut audio_rx, _p) = build_app();
    arm_first_track(&mut app);
    app.handle_event(AppEvent::ToggleMidiRecording);
    tick(&mut app, true, true, samples_at_beat(4.0));
    assert!(app.recording.live, "前提: 録音実体が走っている");
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::ToggleMidiRecording);

    assert!(!app.recording.requested, "Rec は消灯する");
    let sent = drain(&mut audio_rx);
    assert!(
        sent.contains(&AudioCommand::StopRecording),
        "engine の録音セッションを閉じる: {sent:?}"
    );
    assert!(
        !sent.contains(&AudioCommand::Stop),
        "パンチアウトで transport を止めない: {sent:?}"
    );
    assert!(
        app.transport.is_playing,
        "観測値は再生中のまま (engine は走り続けている)"
    );
}

/// engine が止まったのを観測したら、どんな止まり方でも録音セッションが閉じ、
/// プレイヘッドは録音を始めた位置へ戻る。曲末 auto-stop / crash / 書き出しが
/// すべてこの合流点を通る。
#[test]
fn 停止の観測で録音が閉じ再生開始位置へ戻る() {
    let (mut app, mut audio_rx, _p) = build_app();
    arm_first_track(&mut app);
    // beat 4 から再生 → そこでパンチイン (= 停止ホームは 4)。
    app.handle_event(AppEvent::PlayFromCursor { beat: 4.0 });
    tick(&mut app, true, false, samples_at_beat(4.0));
    app.handle_event(AppEvent::ToggleMidiRecording);
    tick(&mut app, true, true, samples_at_beat(9.0));
    assert_eq!(app.transport.playhead_beat, Some(9.0));
    let _ = drain(&mut audio_rx);

    // engine が (曲末なり Stop なりで) 止まったことを観測する。
    tick(&mut app, false, false, samples_at_beat(9.0));

    assert!(!app.recording.requested, "停止したら録音は閉じる");
    assert!(!app.recording.live);
    assert_eq!(
        app.transport.playhead_beat,
        Some(4.0),
        "停止ホーム (録音を始めた位置) へ戻る"
    );
    let sent = drain(&mut audio_rx);
    assert!(
        sent.contains(&AudioCommand::StopRecording),
        "engine 側の録音セッションも閉じる: {sent:?}"
    );
}

/// 録音実体が走っていない間 (count-in / 読み込み待ち / 停止中) の入力は
/// 記録しない。凍ったプレイヘッドへノートが積み上がるのを構造的に防ぐ。
#[test]
fn count_in_中の入力は記録されない() {
    let (mut app, _a, _p) = build_app();
    arm_first_track(&mut app);
    app.handle_event(AppEvent::SetCountInBars(1));
    app.handle_event(AppEvent::ToggleMidiRecording);
    // engine は走り出したが count-in 中 (recording_live=false)。
    tick(&mut app, true, false, 0);

    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 60,
        velocity: 100,
    });
    assert_eq!(
        app.song_doc.song().tracks[0].clips.len(),
        0,
        "count-in 中はクリップも作らない"
    );

    // count-in 明け。
    tick(&mut app, true, true, samples_at_beat(4.0));
    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 60,
        velocity: 100,
    });
    assert_eq!(
        app.song_doc.song().tracks[0].clips.len(),
        1,
        "count-in 明けの入力は記録される"
    );
}

/// 同じ位置・同じ高さのノートが 2 本あっても、note_off は正しい方の長さを
/// 確定する (安定 id で引く)。値照合で探し直していた旧実装は常に 1 本目に
/// 当たり、2 本目が仮の長さ 0.05 拍のまま残っていた。
#[test]
fn 同位置同ピッチでも_note_off_は正しいノートを確定する() {
    let (mut app, _a, _p) = build_app();
    let track_id = arm_first_track(&mut app);
    app.handle_event(AppEvent::ToggleMidiRecording);
    tick(&mut app, true, true, samples_at_beat(0.0));

    // 1 本目: beat 0 で on → beat 1 で off。
    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 60,
        velocity: 100,
    });
    tick(&mut app, true, true, samples_at_beat(1.0));
    app.handle_event(AppEvent::MidiNoteOff { pitch: 60 });

    // 2 本目: 同じ beat 0 へ戻して on → beat 3 で off (= オーバーダブ)。
    // 再生中はプレイヘッドが Tick 追従なので、巻き戻した Tick を届ければよい。
    tick(&mut app, true, true, samples_at_beat(0.0));
    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 60,
        velocity: 100,
    });
    tick(&mut app, true, true, samples_at_beat(3.0));
    app.handle_event(AppEvent::MidiNoteOff { pitch: 60 });

    let notes = recorded_notes(&app);
    assert_eq!(notes.len(), 2, "オーバーダブは重ねたまま残す");
    let mut lengths: Vec<f64> = notes.iter().map(|n| n.duration_beats).collect();
    lengths.sort_by(f64::total_cmp);
    assert_eq!(lengths, vec![1.0, 3.0], "2 本とも実際の長さで確定する");
    let _ = track_id;
}

/// 鍵盤を押したまま録音を終えても、そのノートの長さが確定する。
#[test]
fn 押しっぱなしのノートは録音終了で長さが確定する() {
    let (mut app, _a, _p) = build_app();
    arm_first_track(&mut app);
    app.handle_event(AppEvent::ToggleMidiRecording);
    tick(&mut app, true, true, samples_at_beat(0.0));
    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 64,
        velocity: 100,
    });
    tick(&mut app, true, true, samples_at_beat(2.0));

    // 押したままパンチアウト。
    app.handle_event(AppEvent::ToggleMidiRecording);

    let notes = recorded_notes(&app);
    assert_eq!(notes.len(), 1);
    assert!(
        (notes[0].duration_beats - 2.0).abs() < 1e-9,
        "仮の長さ (0.05 拍) のまま残さない: {}",
        notes[0].duration_beats
    );
}

/// 録音待機トラックは transport 状態に関わらず入力を発音する
/// (インプットモニター)。旧実装はこの経路が無く、弾いても音が鳴らなかった。
#[test]
fn 録音待機トラックは停止中でも弾いた音を鳴らす() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = arm_first_track(&mut app);
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 60,
        velocity: 90,
    });
    let sent = drain(&mut audio_rx);
    assert!(
        sent.contains(&AudioCommand::PreviewNoteOn {
            track_id,
            pitch: 60,
            velocity: 90,
        }),
        "停止中でもモニターへ流す: {sent:?}"
    );

    app.handle_event(AppEvent::MidiNoteOff { pitch: 60 });
    let sent = drain(&mut audio_rx);
    assert!(
        sent.contains(&AudioCommand::PreviewNoteOff {
            track_id,
            pitch: 60
        }),
        "note-off も届く: {sent:?}"
    );

    // arm を外したら、押しっぱなしでも消音する (note-off はもう届かない)。
    app.handle_event(AppEvent::MidiNoteOn {
        pitch: 62,
        velocity: 90,
    });
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::ToggleTrackArmed(track_id));
    let sent = drain(&mut audio_rx);
    assert!(
        sent.contains(&AudioCommand::PreviewNoteOff {
            track_id,
            pitch: 62
        }),
        "arm 解除で鳴らしていた音を止める: {sent:?}"
    );
}
