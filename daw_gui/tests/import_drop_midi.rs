// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #66 (`docs/plan_midi_import.md`): MIDI ファイルの取り込み。
//!
//! headless: 実 D&D の OS イベント (winit `DroppedFile`) は駆動できないので、
//! arrangement view が drop 位置から解決する `ImportTrackTarget` / drop 拍を直接
//! `AppData::handle_event(AppEvent::ImportMidi { .. })` に流し、下流 (track 生成 /
//! clip 配置 / テンポ採用 / 曲長) を検証する。SMF の解析そのもの (PPQ / channel 分割 /
//! 歌詞 / 重なり解消 …) は `daw_gui::midi_import` の単体テスト側。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ImportTrackTarget};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use tempfile::TempDir;

const PPQ: u16 = 480;

fn build_app() -> (
    AppData,
    TempDir,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher;
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    let proj = TempDir::new().unwrap();
    app.song_doc.file_path = Some(proj.path().join("proj.daw"));
    (app, proj, audio_rx, plugin_rx)
}

fn note_on(delta: u32, channel: u8, key: u8, vel: u8) -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::from(delta),
        kind: TrackEventKind::Midi {
            channel: u4::from(channel),
            message: MidiMessage::NoteOn {
                key: u7::from(key),
                vel: u7::from(vel),
            },
        },
    }
}

fn note_off(delta: u32, channel: u8, key: u8) -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::from(delta),
        kind: TrackEventKind::Midi {
            channel: u4::from(channel),
            message: MidiMessage::NoteOff {
                key: u7::from(key),
                vel: u7::from(0),
            },
        },
    }
}

fn eot() -> TrackEvent<'static> {
    TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    }
}

/// `beat` 拍目に `len_beats` 拍の 1 ノートだけを持つ SMF track。
fn one_note_track(at_beat: f64, len_beats: f64, key: u8) -> Vec<TrackEvent<'static>> {
    let ticks = |b: f64| (b * f64::from(PPQ)).round() as u32;
    vec![
        note_on(ticks(at_beat), 0, key, 100),
        note_off(ticks(len_beats), 0, key),
        eot(),
    ]
}

fn write_midi(dir: &Path, name: &str, tracks: Vec<Vec<TrackEvent<'_>>>) -> PathBuf {
    let path = dir.join(name);
    let mut smf = Smf::new(Header::new(
        Format::Parallel,
        Timing::Metrical(u15::from(PPQ)),
    ));
    smf.tracks = tracks;
    smf.save(&path).unwrap();
    path
}

/// 空きスペース drop → 一番下に SMF track の数だけ新規 track ができ、clip は
/// drop 拍に置かれる。既存 track は無変化。
#[test]
fn 空きスペース_drop_で一番下に必要な本数のトラックができる() {
    let (mut app, src, _a, _p) = build_app();
    let mid = write_midi(
        src.path(),
        "two_tracks.mid",
        vec![one_note_track(0.0, 1.0, 60), one_note_track(0.0, 1.0, 67)],
    );
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(8.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), before + 2, "SMF track の数だけ新規 track が増える");
    assert!(tracks[0].clips.is_empty(), "既存の先頭 track は無変化");
    for t in &tracks[before..] {
        assert_eq!(t.clips.len(), 1);
        assert!(
            (t.clips[0].start_beat - 8.0).abs() < 1e-9,
            "clip は drop 拍 (8.0) に置かれる: {}",
            t.clips[0].start_beat
        );
        let content = app
            .song_doc
            .song()
            .clip_contents
            .get(&t.clips[0].content_id)
            .unwrap();
        assert_eq!(content.notes().unwrap().len(), 1, "MIDI content が入っている");
    }
}

/// 既存 track への drop → 1 本目はその track、2 本目以降はその **直下** に挿入。
#[test]
fn 既存トラックへの_drop_は1本目をそこに載せ残りを直下に挿す() {
    let (mut app, src, _a, _p) = build_app();
    // 2 本の track を用意して 2 本目 (index 1) に落とす。
    app.handle_event(AppEvent::AddInstrumentTrack);
    let tracks_before: Vec<u32> = app
        .song_doc
        .song()
        .tracks
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(tracks_before.len(), 2, "前提: track 2 本");
    let mid = write_midi(
        src.path(),
        "three_tracks.mid",
        vec![
            one_note_track(0.0, 1.0, 60),
            one_note_track(0.0, 1.0, 64),
            one_note_track(0.0, 1.0, 67),
        ],
    );

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::Track(1),
        target_beat: Some(0.0),
    });

    let tracks = &app.song_doc.song().tracks;
    assert_eq!(tracks.len(), 4, "既存 2 本 + 新規 2 本");
    assert_eq!(tracks[1].id, tracks_before[1], "1 本目は既存 track に載る");
    assert_eq!(tracks[1].clips.len(), 1);
    assert_eq!(tracks[2].clips.len(), 1, "2 本目は直下に挿入");
    assert_eq!(tracks[3].clips.len(), 1, "3 本目はさらにその下");
    assert!(tracks[0].clips.is_empty(), "上の track は無変化");
}

/// クリップが 1 つも無い曲では SMF のテンポ / 拍子を取り込む
/// (テンポ変化があれば SongTempo automation lane も作る)。
#[test]
fn 空の曲では_smf_のテンポと拍子を取り込む() {
    let (mut app, src, _a, _p) = build_app();
    let meta_track = vec![
        TrackEvent {
            delta: u28::from(0u32),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(400_000u32))), // 150 BPM
        },
        TrackEvent {
            delta: u28::from(0u32),
            kind: TrackEventKind::Meta(MetaMessage::TimeSignature(3, 2, 24, 8)), // 3/4
        },
        TrackEvent {
            delta: u28::from(u32::from(PPQ) * 4),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(500_000u32))), // 120 BPM
        },
        eot(),
    ];
    let mid = write_midi(
        src.path(),
        "tempo.mid",
        vec![meta_track, one_note_track(0.0, 1.0, 60)],
    );
    assert!(
        (app.song_doc.song().bpm - 120.0).abs() < 1e-6,
        "前提: 既定 BPM は 120"
    );

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    let song = app.song_doc.song();
    assert!((song.bpm - 150.0).abs() < 1e-3, "曲頭 BPM を採用: {}", song.bpm);
    assert_eq!(song.time_sig, (3, 4), "拍子も採用");
    let tempo_lane = song
        .song_lanes
        .iter()
        .find(|l| l.target == common::model::AutomationTarget::SongTempo)
        .expect("テンポ変化があるので SongTempo lane ができる");
    assert_eq!(tempo_lane.clips.len(), 1);
    let points = song
        .clip_contents
        .get(&tempo_lane.clips[0].content_id)
        .and_then(|c| c.automation_points())
        .expect("automation content");
    assert_eq!(points.len(), 2, "150 → 120 の 2 点");
    assert!((points[0].value - 150.0).abs() < 1e-3);
    assert!((points[1].value - 120.0).abs() < 1e-3);
    assert!(
        matches!(points[1].curve, common::model::AutomationCurve::Hold),
        "SMF は step tempo なので Hold"
    );
    // 「point が並んでいる」だけでなく、**実際に効く**ことを見る (automation clip の
    // 範囲は半開区間なので、clip 長を最後の point ぴったりにすると全部無効になる)。
    for (beat, want) in [(0.0, 150.0), (3.9, 150.0), (4.0, 120.0), (5.0, 120.0)] {
        let got = common::automation::evaluate_song_tempo(song, beat);
        assert!(
            (got - want).abs() < 1e-3,
            "beat {beat} のテンポは {want} のはず (got {got})"
        );
    }
}

/// SMPTE timing (絶対時刻) の SMF を空の曲へ入れると、テンポは SMF の値を採用し、
/// ノートは **その採用後のテンポでの実時間** に置かれる (換算に使う BPM と再生 BPM が
/// 食い違ってはいけない)。
#[test]
fn smpte_の_midi_はテンポ採用後の実時間で配置される() {
    let (mut app, src, _a, _p) = build_app();
    let path = src.path().join("smpte.mid");
    // 30fps × 80 subframe = 2400 tick/秒。1 秒 (2400 tick) のノート 1 個 + 150 BPM。
    let mut smf = Smf::new(Header::new(
        Format::Parallel,
        Timing::Timecode(midly::Fps::Fps30, 80),
    ));
    smf.tracks = vec![
        vec![
            TrackEvent {
                delta: u28::from(0u32),
                kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(400_000u32))), // 150 BPM
            },
            eot(),
        ],
        vec![note_on(0, 0, 60, 100), note_off(2400, 0, 60), eot()],
    ];
    smf.save(&path).unwrap();

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![path],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    let song = app.song_doc.song();
    assert!((song.bpm - 150.0).abs() < 1e-3, "SMF のテンポを採用: {}", song.bpm);
    let clip = &song.tracks.last().unwrap().clips[0];
    let notes = song.clip_contents.get(&clip.content_id).unwrap().notes().unwrap();
    assert!(
        (notes[0].duration_beats - 2.5).abs() < 1e-3,
        "1 秒のノートは 150 BPM で 2.5 拍 (120 BPM 換算のままなら 2.0 になる): {}",
        notes[0].duration_beats
    );
    assert!(
        song.song_lanes
            .iter()
            .all(|l| l.target != common::model::AutomationTarget::SongTempo),
        "SMPTE は絶対時刻が正本なのでテンポカーブは作らない"
    );
}

/// クリップのある曲では BPM / 拍子を触らない (既存のオーディオ / 動画クリップの
/// 実時間位置がずれるため)。
#[test]
fn クリップのある曲では_bpm_を変えない() {
    let (mut app, src, _a, _p) = build_app();
    let first = write_midi(src.path(), "a.mid", vec![one_note_track(0.0, 1.0, 60)]);
    let second_meta = vec![
        TrackEvent {
            delta: u28::from(0u32),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(300_000u32))), // 200 BPM
        },
        eot(),
    ];
    let second = write_midi(
        src.path(),
        "b.mid",
        vec![second_meta, one_note_track(0.0, 1.0, 72)],
    );

    // 1 回目: 空の曲なので取り込む (このファイルには tempo meta が無いので既定のまま)。
    app.handle_event(AppEvent::ImportMidi {
        paths: vec![first],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });
    let bpm_after_first = app.song_doc.song().bpm;

    // 2 回目: もうクリップがあるので BPM は変わらない。
    app.handle_event(AppEvent::ImportMidi {
        paths: vec![second],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    assert!(
        (app.song_doc.song().bpm - bpm_after_first).abs() < 1e-6,
        "既存クリップがある曲の BPM は据え置き: {} → {}",
        bpm_after_first,
        app.song_doc.song().bpm
    );
    assert!(
        app.song_doc
            .song()
            .song_lanes
            .iter()
            .all(|l| l.target != common::model::AutomationTarget::SongTempo),
        "テンポ lane も作らない"
    );
}

/// clip は「content の窓」。音が 3 小節目から始まる SMF は、clip 先頭が
/// drop 拍 + 8 拍 (4/4 の 2 小節) になり、`content_offset_beats` がその分進む。
/// content-local 拍は SMF tick 0 起点のままなので、`content_origin_beat()` は
/// drop 拍に一致する。
#[test]
fn クリップは音の始まる小節から始まる() {
    let (mut app, src, _a, _p) = build_app();
    let mid = write_midi(
        src.path(),
        "late.mid",
        vec![one_note_track(9.0, 1.0, 60)], // beat 9 (= 3 小節目の 2 拍目)
    );

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(4.0),
    });

    let song = app.song_doc.song();
    let clip = &song.tracks.last().unwrap().clips[0];
    assert!(
        (clip.content_offset_beats - 8.0).abs() < 1e-9,
        "content 窓は 3 小節目 (8 拍) から: {}",
        clip.content_offset_beats
    );
    assert!(
        (clip.start_beat - 12.0).abs() < 1e-9,
        "clip 先頭 = drop 拍 4 + 8 拍: {}",
        clip.start_beat
    );
    assert!(
        (clip.content_origin_beat() - 4.0).abs() < 1e-9,
        "SMF tick 0 は drop 拍に写る"
    );
    assert!(
        (clip.length_beats - 4.0).abs() < 1e-9,
        "9〜10 拍のノートを含む 1 小節: {}",
        clip.length_beats
    );
    let notes = song.clip_contents.get(&clip.content_id).unwrap().notes().unwrap();
    assert!(
        (notes[0].start_beat - 9.0).abs() < 1e-9,
        "content-local 拍は SMF tick 0 起点のまま"
    );
    // 窓の中に発音開始が入っていないと再生されない (sequencer の gate と同じ条件)。
    let (win_start, win_end) = clip.content_window();
    assert!(notes[0].start_beat >= win_start && notes[0].start_beat < win_end);
}

/// 曲の既定長 (64 拍) を超える取り込みでは曲の長さが伸びる
/// (伸ばさないと「全曲」書き出しが 64 拍で切れる)。
#[test]
fn 曲の長さが取り込みに合わせて伸びる() {
    let (mut app, src, _a, _p) = build_app();
    let before = app.song_doc.song().length_beats;
    let mid = write_midi(
        src.path(),
        "long.mid",
        vec![one_note_track(100.0, 4.0, 60)],
    );

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    let after = app.song_doc.song().length_beats;
    assert!(after > before, "曲の長さが伸びる: {before} → {after}");
    assert!((after - 104.0).abs() < 1e-9, "最終ノート終端の小節まで: {after}");
}

/// 取り込み全体が 1 undo ステップ (複数 track / 複数 clip でも 1 回で戻る)。
#[test]
fn 取り込みは1回の_undo_で戻る() {
    let (mut app, src, _a, _p) = build_app();
    let mid = write_midi(
        src.path(),
        "undo.mid",
        vec![one_note_track(0.0, 1.0, 60), one_note_track(0.0, 1.0, 67)],
    );
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![mid],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });
    assert_eq!(app.song_doc.song().tracks.len(), before + 2);

    app.handle_event(AppEvent::Undo);
    assert_eq!(
        app.song_doc.song().tracks.len(),
        before,
        "1 回の Undo で取り込み前に戻る"
    );
}

/// 壊れたファイルは status に出るだけで Song は変わらない。
#[test]
fn 壊れたファイルは曲を変えない() {
    let (mut app, src, _a, _p) = build_app();
    let path = src.path().join("broken.mid");
    std::fs::write(&path, b"this is not a midi file").unwrap();
    let before = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::ImportMidi {
        paths: vec![path],
        target: ImportTrackTarget::NewTrackBottom,
        target_beat: Some(0.0),
    });

    assert_eq!(app.song_doc.song().tracks.len(), before, "track は増えない");
    assert!(
        app.ui_ephemeral.status_message.contains("MIDI import 失敗"),
        "status: {}",
        app.ui_ephemeral.status_message
    );
}
