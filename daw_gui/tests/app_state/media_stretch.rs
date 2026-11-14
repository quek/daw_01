//! r.md #152: 動画クリップ / アニメーション画像クリップの **time-stretch** (Shift + 端 drag)。
//!
//! 伸縮は source の範囲を固定して再生速度を変える — 伸ばす前に各拍で映っていた絵が、伸ばした後の
//! 対応する拍で映る。 ここではコマンド層 (`AppEvent::ResizeClip { stretch: true }`) を通し、
//! preview と書き出しが共有する解決 (`video_playback` / `image_compose`) が返す時刻で確かめる。

use common::audio_render::{TempoMap, event_wave_spans, source_frame_at_beat};
use common::model::{
    AudioContent, AudioEvent, AudioSource, AudioSourcePath, Clip, ClipContent, ClipKey, ImageContent,
    ImageEvent, MidiContent, Note, VideoContent, VideoEvent,
};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::image_compose::active_image_sources_at;
use daw_gui::launcher_time::RowTimeline;
use daw_gui::video_playback::VideoPlaybackEngine;

use super::support::build_app;

const TRACK: u32 = 1;
const CLIP: ClipKey = ClipKey { track_id: TRACK, clip_id: 1 };
/// 120 bpm で 8 拍 = 4 秒。
const SOURCE_MICROS: u64 = 4_000_000;
const SAMPLES: u32 = 64;

/// 120 bpm。 トラック 1 に `content` を拍 `[start, start + len)` のクリップで置く。
fn app_with_clip(content: ClipContent, start: f64, len: f64) -> AppData {
    let (mut app, _a, _p, _d) = build_app();
    app.edit_song(move |song| {
        song.bpm = 120.0;
        let content_id = song.alloc_content(content, String::new());
        let track = &mut song.tracks[0];
        track.clips = vec![Clip { id: 1, start_beat: start, length_beats: len, content_id, ..Clip::default() }];
        track.next_clip_id = 100;
    });
    app
}

fn video_content() -> ClipContent {
    ClipContent::Video(VideoContent {
        events: vec![VideoEvent {
            source_id: 1,
            event_length_beats: 8.0,
            source_end_micros: SOURCE_MICROS,
            ..VideoEvent::default()
        }],
    })
}

fn stretch(app: &mut AppData, target: ClipKey, start_beat: f64, length: f64) {
    app.handle_event(AppEvent::ResizeClip { target, start_beat, length, stretch: true });
}

/// 拍 `[start, start + len)` を等分した各点で映る source 時刻 (µs)。 映らない点があれば panic。
fn video_times(app: &AppData, start: f64, len: f64) -> Vec<u64> {
    let song = app.cur.song_doc.song();
    (0..SAMPLES)
        .map(|i| start + len * (f64::from(i) + 0.5) / f64::from(SAMPLES))
        .map(|beat| {
            let active = VideoPlaybackEngine::active_sources_at(song, &RowTimeline::preview(beat));
            assert_eq!(active.len(), 1, "拍 {beat} は 1 枚だけ映る");
            active[0].source_micros
        })
        .collect()
}

fn assert_same_times(want: &[u64], got: &[u64], label: &str) {
    assert_eq!(want.len(), got.len());
    for (i, (w, g)) in want.iter().zip(got).enumerate() {
        assert!(w.abs_diff(*g) <= 2, "{label} #{i}: source 時刻 {w} / {g}");
    }
}

/// トラック 1 のクリップが見ている video event (分割の片は content を共有するので content ごとに 1 回)。
fn video_events(app: &AppData) -> Vec<VideoEvent> {
    let song = app.cur.song_doc.song();
    let contents: std::collections::BTreeSet<_> = song.tracks[0].clips.iter().map(|c| c.content_id).collect();
    contents
        .iter()
        .filter_map(|id| song.clip_contents.get(id))
        .filter_map(ClipContent::video_events)
        .flatten()
        .cloned()
        .collect()
}

/// 右端を 2 倍へ伸ばすと半分の速さになり、同じ 4 秒が 16 拍に収まる。 分割しても、undo しても保つ。
#[test]
fn 右端の伸縮は映像の速度を変えて同じ範囲を収める() {
    let mut app = app_with_clip(video_content(), 4.0, 8.0);
    let before = video_times(&app, 4.0, 8.0);

    stretch(&mut app, CLIP, 4.0, 16.0);
    let events = video_events(&app);
    assert_eq!(events.len(), 1);
    assert!((events[0].speed - 0.5).abs() < 1e-12, "速度 = 1 / 倍率 (got {})", events[0].speed);
    assert_eq!(events[0].event_length_beats, 16.0);
    assert_eq!(
        (events[0].source_start_micros, events[0].source_end_micros),
        (0, SOURCE_MICROS),
        "source の範囲は動かない"
    );
    assert_same_times(&before, &video_times(&app, 4.0, 16.0), "伸縮後");

    // 伸縮したクリップを割っても、片は同じ速度で同じ絵を映す。
    app.edit_song(|song| {
        song.split_clips_at(9.0);
        song.split_clips_at(15.5);
    });
    let pieces = video_events(&app);
    assert_eq!(pieces.len(), 3);
    assert!(pieces.iter().all(|e| (e.speed - 0.5).abs() < 1e-12), "片は速度を継ぐ");
    assert_same_times(&before, &video_times(&app, 4.0, 16.0), "伸縮後に分割");

    // 分割と伸縮を戻すと等速の元の event。
    app.handle_event(AppEvent::Undo);
    app.handle_event(AppEvent::Undo);
    assert_eq!(video_events(&app)[0].speed, 1.0);
    assert_same_times(&before, &video_times(&app, 4.0, 8.0), "undo 後");
}

/// 左端の伸縮は右端が pivot。 先に割ってある片 (take の頭が event の外) と、左端を trim してある
/// クリップ (窓が content の途中から始まる) でも、絵は拍に比例して写る。
#[test]
fn 左端の伸縮と分割済み_trim_済みの映像も同じ絵を映す() {
    // 分割済み: content の中で 3 拍目と 5.5 拍目に切れ目。
    let mut app = app_with_clip(video_content(), 4.0, 8.0);
    let before = video_times(&app, 4.0, 8.0);
    app.edit_song(|song| {
        let content_id = song.tracks[0].clips[0].content_id;
        song.split_content_at_points(content_id, &[3.0, 5.5]);
    });
    assert_eq!(video_events(&app).len(), 3);
    // 左端を 4 → 8 拍へ (右端 12 拍が固定、長さ半分 = 2 倍速)。
    stretch(&mut app, CLIP, 8.0, 4.0);
    assert!(video_events(&app).iter().all(|e| (e.speed - 2.0).abs() < 1e-12));
    assert_same_times(&before, &video_times(&app, 8.0, 4.0), "分割済みを左端で縮める");

    // trim 済み: 左端を 2 拍 trim して窓を [6, 12) にしてから、右端を 2 倍に伸ばす。
    let mut app = app_with_clip(video_content(), 4.0, 8.0);
    app.handle_event(AppEvent::ResizeClip { target: CLIP, start_beat: 6.0, length: 6.0, stretch: false });
    let before = video_times(&app, 6.0, 6.0);
    stretch(&mut app, CLIP, 6.0, 12.0);
    assert_same_times(&before, &video_times(&app, 6.0, 12.0), "trim 済みを右端で伸ばす");
    // 左端を戻す (trim) と、隠れていた頭も同じ速度で続いている。
    app.handle_event(AppEvent::ResizeClip { target: CLIP, start_beat: 4.0, length: 14.0, stretch: false });
    let head = VideoPlaybackEngine::active_sources_at(app.cur.song_doc.song(), &RowTimeline::preview(4.0));
    assert_eq!(head[0].source_micros, 500_000, "拍 4 = 伸縮前の content 1 拍目 (0.5 秒)");
}

/// アニメーション画像は経過秒 × 速度でフレームを選ぶ。 2 倍へ伸ばすと同じフレームが 2 倍の拍に写る。
#[test]
fn 画像クリップの伸縮はアニメーションの速度を変える() {
    let content = ClipContent::Image(ImageContent {
        events: vec![ImageEvent { source_id: 1, event_length_beats: 8.0, ..ImageEvent::default() }],
    });
    let mut app = app_with_clip(content, 0.0, 8.0);
    let anim_secs = |app: &AppData, beat: f64| {
        let frames = active_image_sources_at(
            app.cur.song_doc.song(),
            &RowTimeline::preview(beat),
            common::mod_plane::ModPlaneRef::default(),
            None,
        );
        assert_eq!(frames.len(), 1, "拍 {beat} は 1 枚だけ映る");
        frames[0].anim_secs
    };
    assert!((anim_secs(&app, 3.0) - 1.5).abs() < 1e-9, "等速: 3 拍 = 1.5 秒");

    stretch(&mut app, CLIP, 0.0, 16.0);
    assert!((anim_secs(&app, 6.0) - 1.5).abs() < 1e-9, "半分の速さ: 6 拍で 1.5 秒ぶん進む");
    assert!((anim_secs(&app, 15.0) - 3.75).abs() < 1e-9, "伸ばした先まで event が続く");

    // 左端を 0 → 12 拍へ (右端 16 拍が固定、長さ 1/4): event は新しい左端から始まり、速度は 4 倍
    // (0.5 → 2.0) になる。
    stretch(&mut app, CLIP, 12.0, 4.0);
    assert!((anim_secs(&app, 13.0) - 1.0).abs() < 1e-9, "1 拍 (0.5 秒) × 2 倍速 = 1 秒ぶん");
}

/// 左端を trim したクリップの伸縮は、窓より前に隠れている中身も比例して写す (content の種類に
/// 依らない規則)。 以前は隠れている中身を窓の頭へ切り詰めていたので、隠れていたノートが窓の頭で
/// 鳴り、見えていた中身が跳んでいた。
#[test]
fn 左端を_trim_したクリップの伸縮は隠れている中身を窓へ出さない() {
    let note = |id, start_beat| Note { id, start_beat, duration_beats: 0.5, pitch: 60, velocity: 100, ..Note::default() };
    let content = ClipContent::Midi(MidiContent { notes: vec![note(1, 0.0), note(2, 3.0)], next_note_id: 3 });
    let mut app = app_with_clip(content, 4.0, 8.0);
    // 窓を [6, 12) に: content 0 拍のノートは隠れ、3 拍のノートは song 7 拍で鳴る。
    app.handle_event(AppEvent::ResizeClip { target: CLIP, start_beat: 6.0, length: 6.0, stretch: false });
    stretch(&mut app, CLIP, 6.0, 12.0);

    let song = app.cur.song_doc.song();
    let clip = song.clip_by_key(CLIP).expect("clip");
    let notes = song.clip_contents[&clip.content_id].notes().expect("midi");
    let sounding: Vec<f64> = notes
        .iter()
        .filter(|n| clip.window_has_onset(n.start_beat))
        .map(|n| clip.content_to_song_beat(n.start_beat))
        .collect();
    assert_eq!(sounding, vec![8.0], "見えていたノートだけが、左端 (6 拍) から 2 倍の距離で鳴る");
    assert!(notes.iter().all(|n| n.start_beat >= 0.0), "content-local 拍は負にならない");
}

/// 動画と、その取り込みで出来る対の音声クリップ (既定の Stretch) を同じ比率で伸縮すると、
/// どの拍でも映像と音声が同じ source 時刻を読む (同期が保たれる)。
#[test]
fn 動画と対の音声を同じ比率で伸縮しても同期する() {
    const AUDIO: ClipKey = ClipKey { track_id: TRACK, clip_id: 2 };
    const FRAMES: u64 = 4 * 48_000;
    let mut app = app_with_clip(video_content(), 0.0, 8.0);
    app.edit_song(|song| {
        let src = song.alloc_audio_source_id();
        song.media.audio_sources.insert(
            src,
            AudioSource {
                path: AudioSourcePath::Generated { id: 1 },
                sample_rate: 48_000,
                channels: 2,
                frames: FRAMES,
                original_bpm: None,
                root_key: None,
            },
        );
        let content_id = song.alloc_content(
            ClipContent::Audio(AudioContent {
                events: vec![AudioEvent {
                    id: 1,
                    source_id: src,
                    event_length_beats: 8.0,
                    source_end_frames: FRAMES,
                    ..AudioEvent::default()
                }],
                next_event_id: 2,
            }),
            String::new(),
        );
        // 同じトラックの離れた位置に置く (伸縮の比率だけを見るので行は問わない)。
        song.tracks[0].clips.push(Clip {
            id: 2,
            start_beat: 32.0,
            length_beats: 8.0,
            content_id,
            ..Clip::default()
        });
    });
    stretch(&mut app, CLIP, 0.0, 12.0);
    stretch(&mut app, AUDIO, 32.0, 12.0);

    let song = app.cur.song_doc.song();
    let audio = song.clip_by_key(AUDIO).expect("audio clip");
    let event = song.clip_contents[&audio.content_id].audio_events().expect("audio")[0].clone();
    let mut spans = Vec::new();
    event_wave_spans(&event, 48_000, &TempoMap::constant(120.0), 32.0, &mut spans);
    for (i, video_micros) in video_times(&app, 0.0, 12.0).into_iter().enumerate() {
        let local = 12.0 * (f64::from(i as u32) + 0.5) / f64::from(SAMPLES);
        let audio_secs = source_frame_at_beat(&spans, local).expect("鳴っている") / 48_000.0;
        let video_secs = video_micros as f64 / 1_000_000.0;
        assert!((audio_secs - video_secs).abs() < 1e-3, "拍 {local}: 音 {audio_secs}s / 映像 {video_secs}s");
    }
}
