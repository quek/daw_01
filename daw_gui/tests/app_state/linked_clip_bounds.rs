//! r.md #44 の回帰テスト: **リンクしたクリップでも開始・終了は個別**。
//!
//! clip は共有 content への「窓」 (`Clip::content_offset_beats` +
//! `length_beats`)。 端 trim は clip 側 3 フィールドだけを書き換え、content には
//! 一切触れない — これが破れると片方を trim した瞬間に相方の波形・再生範囲まで
//! 変わる (`docs/plan_clip_content_window.md` §1)。

use common::model::{AudioContent, AudioEvent, Clip, ClipContent, MidiContent, Note};

use daw_gui::app::AppEvent;
use daw_gui::app_types::ClipKey;

use super::support;

const A: ClipKey = ClipKey { track_id: TRACK_ID, clip_id: 1 };
const B: ClipKey = ClipKey { track_id: TRACK_ID, clip_id: 2 };
/// `support::build_app` の既定トラックの id (住所は index ではなく安定 id)。
/// 起動時の 1 本目は allocator から採るので 1 (r.md #87 — id 0 は未採番の
/// sentinel で、実トラックの住所に使うとランチャーの行キーと衝突する)。
const TRACK_ID: u32 = 1;

/// 48kHz / 120bpm → 1 拍 = 24000 frame。 8 拍ぶんの source を丸ごと鳴らす
/// audio event 1 つを、content を共有する 2 clip (A: 0 拍 / B: 16 拍) が見る。
fn app_with_two_linked_audio_clips() -> daw_gui::app::AppData {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let src = song.alloc_audio_source_id();
        song.media.audio_sources.insert(
            src,
            common::model::AudioSource {
                path: common::model::AudioSourcePath::Generated { id: 1 },
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
            "shared".to_string(),
        );
        song.tracks[0].clips = vec![
            Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id: cid, ..Default::default() },
            Clip { id: 2, start_beat: 16.0, length_beats: 8.0, content_id: cid, ..Default::default() },
        ];
    });
    app.song_doc.mark_saved();
    app
}

/// 共有 content の唯一の audio event を返す。
fn shared_event(app: &daw_gui::app::AppData) -> AudioEvent {
    let song = app.song_doc.song();
    let cid = song.tracks[0].clips[0].content_id;
    match song.clip_contents.get(&cid) {
        Some(ClipContent::Audio(a)) => a.events[0].clone(),
        other => panic!("audio content が無い: {other:?}"),
    }
}

fn clip(app: &daw_gui::app::AppData, r: ClipKey) -> Clip {
    // 住所は安定 id (index ではない)。
    app.song_doc.song().clip_by_key(r).expect("clip exists").clone()
}

/// A の右端を 8 → 4 拍に縮めても、共有 content と相方 B の窓は一切変わらない。
#[test]
fn right_trim_of_a_linked_audio_clip_leaves_the_sibling_untouched() {
    let mut app = app_with_two_linked_audio_clips();
    let before = shared_event(&app);

    app.handle_event(AppEvent::ResizeClip {
        target: A,
        start_beat: 0.0,
        length: 4.0,
        stretch: false,
    });

    assert_eq!(
        shared_event(&app),
        before,
        "端 trim は共有 content を書き換えてはならない (書き換えると linked clip を巻き込む)"
    );
    let b = clip(&app, B);
    assert_eq!((b.start_beat, b.length_beats, b.content_offset_beats), (16.0, 8.0, 0.0));
    let a = clip(&app, A);
    assert_eq!(
        (a.start_beat, a.length_beats, a.content_offset_beats),
        (0.0, 4.0, 0.0),
        "縮んだのは A の窓だけ"
    );
    assert_eq!(a.content_id, b.content_id, "trim でリンクは切れない");
}

/// A の左端を 0 → 2 拍へ trim すると、窓だけが content 上を進む
/// (= 中身は song 上の同じ位置に留まる)。 相方 B は無傷。
#[test]
fn left_trim_moves_the_window_not_the_content() {
    let mut app = app_with_two_linked_audio_clips();
    let before = shared_event(&app);

    app.handle_event(AppEvent::ResizeClip {
        target: A,
        start_beat: 2.0,
        length: 6.0,
        stretch: false,
    });

    assert_eq!(shared_event(&app), before, "左端 trim も content を触らない");
    let a = clip(&app, A);
    assert_eq!((a.start_beat, a.length_beats, a.content_offset_beats), (2.0, 6.0, 2.0));
    // content 原点 = start - offset は不動 = 中身が song 上で動いていない。
    assert!(
        (a.content_origin_beat() - 0.0).abs() < 1e-9,
        "content 原点は動かない (got {})",
        a.content_origin_beat()
    );
    let b = clip(&app, B);
    assert_eq!((b.start_beat, b.length_beats, b.content_offset_beats), (16.0, 8.0, 0.0));
}

/// 左端を戻すと、trim で隠れていた中身がそのまま復帰する
/// (content を破壊していないので往復が可逆)。
#[test]
fn left_trim_is_reversible_because_content_is_never_destroyed() {
    let mut app = app_with_two_linked_audio_clips();
    let before = shared_event(&app);

    app.handle_event(AppEvent::ResizeClip { target: A, start_beat: 3.0, length: 5.0, stretch: false });
    app.handle_event(AppEvent::ResizeClip { target: A, start_beat: 0.0, length: 8.0, stretch: false });

    assert_eq!(shared_event(&app), before, "往復しても content は元のまま");
    let a = clip(&app, A);
    assert_eq!((a.start_beat, a.length_beats, a.content_offset_beats), (0.0, 8.0, 0.0));
}

/// MIDI clip も同じ規律: 左端 trim でノートは動かず (song 上の位置を保ち)、
/// 隠れるだけ。 旧実装は clip 全体が右へずれてノートまで移動していた。
#[test]
fn left_trim_of_a_midi_clip_hides_notes_without_moving_them() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let cid = song.alloc_content(
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note { id: 1, start_beat: 0.0, duration_beats: 1.0, pitch: 60, velocity: 100, lyric: None, muted: false },
                    Note { id: 2, start_beat: 4.0, duration_beats: 1.0, pitch: 64, velocity: 100, lyric: None, muted: false },
                ],
                next_note_id: 3,
            }),
            "midi".to_string(),
        );
        song.tracks[0].clips = vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            ..Default::default()
        }];
    });
    app.song_doc.mark_saved();
    let cid = app.song_doc.song().tracks[0].clips[0].content_id;
    let before = app.song_doc.song().clip_contents.get(&cid).cloned();

    app.handle_event(AppEvent::ResizeClip { target: A, start_beat: 2.0, length: 6.0, stretch: false });

    assert_eq!(
        app.song_doc.song().clip_contents.get(&cid).cloned(),
        before,
        "MIDI の左端 trim も notes を動かさない"
    );
    let a = clip(&app, A);
    assert_eq!((a.start_beat, a.length_beats, a.content_offset_beats), (2.0, 6.0, 2.0));
    // 4 拍目の note は content 原点 0 + 4 = song 4 拍のまま (窓 [2,8) の内側)。
    assert!((a.content_to_song_beat(4.0) - 4.0).abs() < 1e-9);
}

/// 共有コピー (D / Ctrl+drag) は **窓ごと** 複製する。 trim 済み clip を複製したら
/// 見えている範囲が同じでなければならない。
#[test]
fn shared_duplicate_carries_the_window() {
    let mut app = app_with_two_linked_audio_clips();
    app.handle_event(AppEvent::ResizeClip { target: A, start_beat: 2.0, length: 4.0, stretch: false });
    app.handle_event(AppEvent::SelectClip { target: A, additive: false });
    // D = 範囲を 1 つ後ろへ複製 (`docs/plan_range_selection.md` §6)。 窓 [2,6) が
    // そのまま [6,10) へ写る。
    app.copy_time_range(2.0, 6.0, 4.0, &[(A.track_id, A.track_id)], false);

    let clips = &app.song_doc.song().tracks[0].clips;
    assert_eq!(clips.len(), 3, "複製 clip が末尾に 1 本増える");
    let dup = &clips[2];
    assert_eq!(
        (dup.length_beats, dup.content_offset_beats),
        (4.0, 2.0),
        "複製は元と同じ窓 (長さ + content 上の位置) を見せる"
    );
    assert_eq!(dup.content_id, clips[0].content_id, "共有コピーなので content は同じ");
}
