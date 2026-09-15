//! r.md #132 残件: **分割は切れ目を入れるだけ** — 分割した片が元と違う結果を出さない (コマンド層で通す)。
//!
//! - 読み上げの Text クリップ: 読み上げは最初の片の 1 回だけ (2026-09-15 ユーザー決定)。 片を動かしても
//!   読み直さず、Glue (`J`) で元の 1 つに戻り、undo で分割前に戻る。
//! - 歌唱の MIDI クリップ: 分割の片 (同じ content を別の窓で見る) を動かしても、窓の外に隠れている前後の
//!   ノートを動かした先で歌わない。
//! - オーディオエディタで割った片: 端 trim は窓を動かすだけ (隠れていた続きの音が戻る)、移調は片自身の
//!   頭を起点に効く。
//!
//! 音そのものの一致は `daw_audio` の `split_fidelity_tests`、字幕 / 映像は `text_compose` /
//! `video_playback` の unit test が確かめる。

use common::audio_render::{TempoMap, event_wave_spans, source_frame_at_beat};
use common::model::{
    AudioContent, AudioEvent, Clip, ClipContent, ClipKey, Device, LaneRef, MidiContent, Note, PluginInstance,
    StretchMode, TextContent, TextEvent,
};
use common::plugin_metadata::{NoteMetadata, TalkMetadata};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_range::{RangeEvent, RangeMoveMode, RowDest};
use daw_gui::event_split::{SplitAt, SplitJoinEvent, SplitSurface};

use super::support::{build_app, drain};

const TRACK: u32 = 1;
const CLIP: ClipKey = ClipKey { track_id: TRACK, clip_id: 1 };
const VOICEVOX_DEVICE: u64 = 900;
/// アレンジのスナップの選択肢 `2 拍` (`view::snap::SNAP_LABELS` の index)。
const GRID_2_BEATS: u8 = 0;

/// トラック 1 を VOICEVOX の vocal トラックにして、`content` を拍 0 から `len` 拍のクリップで置く。
fn vocal_app(content: ClipContent, len: f64) -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, _audio, plugin_rx, _d) = build_app();
    app.edit_song(move |song| {
        let content_id = song.alloc_content(content, String::new());
        let track = &mut song.tracks[0];
        track.devices.push(Device::Plugin(PluginInstance {
            id: VOICEVOX_DEVICE,
            ..PluginInstance::with_ports(
                common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                common::plugin_format::PluginFormat::Builtin,
                common::port_config::PortConfig { has_note_input: true, has_audio_output: true, ..Default::default() },
            )
        }));
        track.clips = vec![Clip { id: 1, start_beat: 0.0, length_beats: len, content_id, ..Clip::default() }];
        track.next_clip_id = 100;
    });
    app.cur.pipc.loaded_devices.insert(
        VOICEVOX_DEVICE,
        daw_gui::app::LoadedDeviceInfo {
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
            token: common::protocol::InstanceToken(1),
        },
    );
    (app, plugin_rx)
}

/// 送り直しを flush して、builtin VOICEVOX が今持っている歌唱 / 読み上げの一覧を返す (変わっていなければ
/// 送り直されないので、最後に送った一覧を `last` に持ち回る)。
fn synced(
    app: &mut AppData,
    rx: &mut UnboundedReceiver<PluginCommand>,
    last: &mut (Vec<NoteMetadata>, Vec<TalkMetadata>),
) -> (Vec<NoteMetadata>, Vec<TalkMetadata>) {
    app.flush_song_sync();
    for msg in drain(rx) {
        if let PluginCommand::SetBuiltinPluginNoteMetadata { entries, talk, .. } = msg {
            *last = (entries, talk);
        }
    }
    last.clone()
}

fn split_clip_on_grid(app: &mut AppData) {
    app.cur.view.arrange_snap_choice = GRID_2_BEATS;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.arrangement_hover_clip = Some(CLIP);
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Split { surface: SplitSurface::Clips, at: SplitAt::Grid }));
    app.cur.peph.arrangement_hover_clip = None;
}

fn text_events(app: &AppData) -> Vec<TextEvent> {
    let song = app.cur.song_doc.song();
    let mut clips: Vec<&Clip> = song.tracks[0].clips.iter().collect();
    clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    let mut ids: Vec<u32> = clips.iter().map(|c| c.content_id).collect();
    ids.dedup();
    ids.iter()
        .filter_map(|id| song.clip_contents.get(id).and_then(|c| c.text_events()))
        .flat_map(|events| events.iter().cloned())
        .collect()
}

#[test]
fn 読み上げは最初の片だけが読み_動かしても読み直さず_glue_で元に戻る() {
    let original = TextEvent {
        text: "今日はいい天気ですね".into(),
        event_length_beats: 8.0,
        fade_in_beats: 3.0,
        fade_out_beats: 2.5,
        ..TextEvent::default()
    };
    let (mut app, mut rx) = vocal_app(ClipContent::Text(TextContent { events: vec![original.clone()] }), 8.0);
    let mut last = Default::default();
    let readings = |talk: &[TalkMetadata]| talk.iter().map(|t| (t.start_beat, t.text.clone())).collect::<Vec<_>>();
    let once = vec![(0.0, original.text.clone())];
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), once, "前提: 分割前は 1 回");

    // Shift+E で 2 拍ごとに 4 片。 片は同じ content を別の窓で見る。
    split_clip_on_grid(&mut app);
    assert_eq!(app.cur.song_doc.song().tracks[0].clips.len(), 4);
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), once, "読み上げは最初の片の 1 回だけ");
    let marks: Vec<bool> = text_events(&app).iter().map(|e| e.continuation).collect();
    assert_eq!(marks, vec![false, true, true, true], "続きの片は読み上げない印を持つ");

    // 続きの片 (拍 4〜6) を拍 16 へ動かしても、動かした先で読み直さない (クリップヘッダの drag の確定と同じ
    // event なので、1 回の undo でこの移動だけが戻る)。
    app.handle_event(AppEvent::Range(RangeEvent::Move {
        range: (4.0, 6.0),
        delta_beats: 12.0,
        rows: vec![(TRACK, RowDest::Track(TRACK))],
        mode: RangeMoveMode::Move,
    }));
    assert!(app.cur.song_doc.song().tracks[0].clips.iter().any(|c| c.start_beat == 16.0), "前提: 片が動いた");
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), once, "動かした続きの片は読まない");
    app.cur.song_doc.undo();
    assert_eq!(text_events(&app).len(), 4, "undo で移動だけが戻る");

    // 範囲 [0, 8) で J → 元の 1 つ (本文・位置・長さ・fade・読み上げる印) に戻る。
    app.handle_event(AppEvent::SetTimeSelection { start_beat: 0.0, end_beat: 8.0, lanes: vec![LaneRef::Track(TRACK)] });
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Join { surface: SplitSurface::Clips }));
    assert_eq!(app.cur.song_doc.song().tracks[0].clips.len(), 1, "{}", app.ui_ephemeral.status_message);
    assert_eq!(text_events(&app), vec![original.clone()], "Glue で分割前の 1 つに戻る");
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), once);

    // undo を 2 回 (Glue / 分割) で分割前の姿そのもの。
    app.cur.song_doc.undo();
    assert_eq!(text_events(&app).len(), 4);
    app.cur.song_doc.undo();
    assert_eq!(text_events(&app), vec![original]);
}

#[test]
fn 歌唱の片を動かしても隠れている前後のノートを歌わない() {
    let note = |id: u32, start: f64, len: f64, lyric: &str| Note {
        id,
        start_beat: start,
        duration_beats: len,
        pitch: 64,
        velocity: 100,
        lyric: Some(lyric.into()),
        muted: false,
    };
    let notes = vec![note(1, 0.0, 3.0, "あ"), note(2, 4.0, 2.0, "い"), note(3, 6.0, 2.0, "う")];
    let (mut app, mut rx) = vocal_app(ClipContent::Midi(MidiContent { notes, next_note_id: 4 }), 8.0);
    let mut last = Default::default();
    let sung = |entries: &[NoteMetadata]| {
        let mut v: Vec<(f64, f64, String)> =
            entries.iter().map(|e| (e.start_beat, e.duration_beats, e.lyric.clone())).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v
    };
    let before = sung(&synced(&mut app, &mut rx, &mut last).0);
    assert_eq!(before.len(), 3);

    // 2 拍ごとに割り (跨ぐ「あ」は「あ」+「ー」)、拍 4〜8 の片を拍 12 へ。 残った片は「あ」「ー」、
    // 動かした片は「い」「う」だけを動かした先で歌う (content は共有したまま、窓の外は歌わない)。
    split_clip_on_grid(&mut app);
    app.handle_event(AppEvent::Range(RangeEvent::Move {
        range: (4.0, 8.0),
        delta_beats: 8.0,
        rows: vec![(TRACK, RowDest::Track(TRACK))],
        mode: RangeMoveMode::Move,
    }));
    let after = sung(&synced(&mut app, &mut rx, &mut last).0);
    assert_eq!(
        after,
        vec![(0.0, 2.0, "あ".into()), (2.0, 1.0, "ー".into()), (12.0, 2.0, "い".into()), (14.0, 2.0, "う".into())],
        "動かした片は自分の窓のノートだけを動かした先で歌う"
    );
}

/// オーディオエディタで割った片: 端 trim は **窓だけ** を動かす (見えている音は動かず、伸ばし戻すと隠れて
/// いた続きの音がそのまま現れる)。 移調は片自身の頭を起点に効く (take の頭 = 前の片の頭を起点にしない)。
#[test]
fn オーディオエディタの片の_trim_は窓を動かし_移調は片の頭を起点に効く() {
    let (mut app, _audio, _plugin, _d) = build_app();
    app.edit_song(|song| {
        song.media.audio_sources.insert(
            1,
            common::model::AudioSource {
                path: common::model::AudioSourcePath::Absolute("C:/split-fidelity/a.wav".into()),
                sample_rate: 48_000,
                channels: 1,
                frames: 192_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let event = AudioEvent {
            id: 1,
            source_id: 1,
            event_length_beats: 6.0,
            source_start_frames: 24_000,
            source_end_frames: 96_000,
            stretch_mode: StretchMode::Repitch,
            ..AudioEvent::default()
        };
        let content_id = song.alloc_content(ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), String::new());
        song.tracks[0].clips = vec![Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id, ..Clip::default() }];
    });
    let events = |app: &AppData| -> Vec<AudioEvent> {
        let song = app.cur.song_doc.song();
        let clip = song.clip_by_key(CLIP).expect("clip");
        song.clip_contents[&clip.content_id].audio_events().expect("audio").to_vec()
    };
    // その event が見えている拍 `local` で読む source frame (描画と engine が共有する写像)。
    let reads = |ev: &AudioEvent, local: f64| {
        let mut spans = Vec::new();
        event_wave_spans(ev, 48_000, &TempoMap::constant(120.0), ev.event_start_in_clip_beats, &mut spans);
        source_frame_at_beat(&spans, local).expect("鳴っている")
    };
    let whole = events(&app)[0].clone();
    app.handle_event(AppEvent::OpenAudioEditor(CLIP));
    app.cur.view.arrange_snap_choice = GRID_2_BEATS;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.audio_editor_hover_beat_in_clip = Some(1.0);
    app.cur.peph.audio_editor_zoom_x = 100.0;
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Split { surface: SplitSurface::Clips, at: SplitAt::Grid }));
    assert_eq!(events(&app).len(), 3, "2 拍ごとに 3 片");

    // 最初の片 [0, 2) の右端を 1 拍内側へ縮めてから 1 拍伸ばし戻す → 隠れていた拍 1〜2 が分割前の音で戻る。
    let trim = |app: &mut AppData, event_idx: usize, side, delta_beats| {
        app.handle_event(AppEvent::SetAudioEventTrim { clip: CLIP, event_idx, side, delta_beats });
    };
    use daw_gui::event::AudioEventTrimSide::{Left, Right};
    trim(&mut app, 0, Right, -1.0);
    trim(&mut app, 0, Right, 1.0);
    let first = events(&app)[0].clone();
    assert_eq!((first.event_start_in_clip_beats, first.event_length_beats), (0.0, 2.0));
    for local in [0.25, 1.25, 1.75] {
        let (got, want) = (reads(&first, local), reads(&whole, local));
        assert!((got - want).abs() <= 2.0, "伸ばし戻した片の拍 {local}: {got} / 分割前 {want}");
    }
    // 最後の片 [4, 6) の左端を 1 拍内側へ → 見えている頭は分割前の拍 5 の音。 右端を take の外 (素材の続き)
    // まで 1 拍伸ばす → 伸ばした拍 6〜7 は同じ伸縮率で素材の続きを読む。
    trim(&mut app, 2, Left, 1.0);
    trim(&mut app, 2, Right, 1.0);
    let last = events(&app)[2].clone();
    assert_eq!((last.event_start_in_clip_beats, last.event_length_beats), (5.0, 2.0));
    assert!((reads(&last, 0.1) - reads(&whole, 5.1)).abs() <= 2.0, "左 trim は窓だけを動かす");
    let rate = (reads(&whole, 5.5) - reads(&whole, 5.0)) / 0.5;
    assert!((reads(&last, 1.5) - (reads(&whole, 5.0) + rate * 1.5)).abs() <= 2.0, "take の外は同じ伸縮率で続く");

    // 真ん中の片 [2, 4) だけを +12 半音: 片の頭の音は変わらず、そこから 2 倍速で読む。
    app.handle_event(AppEvent::SetAudioEditorEventSelection(vec![1]));
    let middle_before = events(&app)[1].clone();
    app.handle_event(AppEvent::SetClipPitchSemitones { target: CLIP, semitones: 12.0 });
    let pitched = events(&app)[1].clone();
    assert_eq!(pitched.pitch_semitones, 12.0);
    assert!((reads(&pitched, 0.0) - reads(&middle_before, 0.0)).abs() <= 2.0, "片の頭の音は移調で跳ばない");
    let speed =
        (reads(&pitched, 0.5) - reads(&pitched, 0.0)) / (reads(&middle_before, 0.5) - reads(&middle_before, 0.0));
    assert!((speed - 2.0).abs() < 0.01, "片の頭から 2 倍速で読む: {speed}");
    assert_eq!(events(&app)[0], first, "選んでいない片は変わらない");
    assert_eq!(events(&app)[2], last, "選んでいない片は変わらない");
}
