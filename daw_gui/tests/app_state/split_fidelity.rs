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

/// 字幕の「ここから読む」: 続きの片を普通の読み上げに戻すと、その片の窓の頭から読み、後ろの片は続きのまま。
/// 1 回の undo で戻り、同じトグルで続きの片に戻せる (ノートの歌詞を「ー」に書き換えられるのと同じ対称性)。
#[test]
fn 続きの片は_ここから読む_で読み上げに戻せて_undo_で戻る() {
    let original = TextEvent { text: "おはようございます".into(), event_length_beats: 8.0, ..TextEvent::default() };
    let (mut app, mut rx) = vocal_app(ClipContent::Text(TextContent { events: vec![original] }), 8.0);
    let mut last = Default::default();
    let readings = |talk: &[TalkMetadata]| talk.iter().map(|t| t.start_beat).collect::<Vec<_>>();
    split_clip_on_grid(&mut app);
    let pieces = sorted_clip_keys(&app);
    let reads: Vec<Option<bool>> = pieces.iter().map(|&k| app.clip_text_reads(k)).collect();
    assert_eq!(reads, vec![Some(true), Some(false), Some(false), Some(false)], "続きの片 (アレンジに「続き」の印) は 3 つ");
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), vec![0.0]);

    // 2 つ目の片 (拍 2〜4) から読む → 拍 0 と拍 2 で読み、3 つ目以降は続きのまま。
    app.handle_event(AppEvent::SetClipTextReads { clip: pieces[1], reads: true });
    let reads: Vec<Option<bool>> = pieces.iter().map(|&k| app.clip_text_reads(k)).collect();
    assert_eq!(reads, vec![Some(true), Some(true), Some(false), Some(false)]);
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), vec![0.0, 2.0], "選んだ片の頭から読む");
    assert_eq!(app.cur.song_doc.history_labels().last().copied(), Some("ここから読む"));

    app.cur.song_doc.undo();
    assert_eq!(app.clip_text_reads(pieces[1]), Some(false), "1 回の undo で続きの片に戻る");
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), vec![0.0]);

    // 最初の片を続きにすると、どこも読まない (字幕だけ)。 戻すとまた読む。
    app.handle_event(AppEvent::SetClipTextReads { clip: pieces[0], reads: false });
    assert!(readings(&synced(&mut app, &mut rx, &mut last).1).is_empty(), "続きの片だけの字幕は読まない");
    app.handle_event(AppEvent::SetClipTextReads { clip: pieces[0], reads: true });
    assert_eq!(readings(&synced(&mut app, &mut rx, &mut last).1), vec![0.0]);
}

/// 分割した字幕クリップの rename (= 本文の編集) は **そのクリップの窓の片** だけを書き換え、編集の口
/// (rename の初期値と「変わっていなければ何もしない」判定) も同じ片から読む。 content の先頭の片を読むと、
/// 片ごとに本文が違うとき rename の初期値が別の片の本文になり、その本文へ戻す rename が黙って無視される。
#[test]
fn 分割した字幕クリップの_rename_は窓の片を書き換えその片から読む() {
    let original = TextEvent { text: "はじめまして".into(), event_length_beats: 8.0, ..TextEvent::default() };
    let (mut app, _rx) = vocal_app(ClipContent::Text(TextContent { events: vec![original.clone()] }), 8.0);
    split_clip_on_grid(&mut app);
    let pieces = sorted_clip_keys(&app);
    let rename = |app: &mut AppData, clip: ClipKey, name: &str| {
        app.handle_event(AppEvent::BeginRenameClip(clip));
        app.handle_event(AppEvent::RenameClipChanged(name.into()));
        app.handle_event(AppEvent::CommitRenameClip);
    };
    let texts = |app: &AppData| text_events(app).iter().map(|e| e.text.clone()).collect::<Vec<_>>();

    rename(&mut app, pieces[1], "よろしく");
    let renamed = vec![original.text.clone(), "よろしく".into(), original.text.clone(), original.text.clone()];
    assert_eq!(texts(&app), renamed, "書き換わるのは 2 つ目のクリップの窓の片だけ");

    app.handle_event(AppEvent::BeginRenameClip(pieces[1]));
    assert_eq!(app.cur.peph.clip_rename_text, "よろしく", "rename の初期値はそのクリップの窓の本文");
    app.handle_event(AppEvent::CancelRenameClip);

    rename(&mut app, pieces[1], &original.text);
    assert_eq!(texts(&app), vec![original.text.clone(); 4], "別の片と同じ本文へ戻す rename も効く");
}

/// ARA (Melodyne) トラックを割ると、片は **同じ audio modification** の region になり (Melodyne の編集が続く)、
/// region は窓に見えている片だけを、分割前の region をちょうど切り分けた位置 (再生位置と modification の中の
/// 位置) に置く。 persistent id は安定 id から作るので、片を動かしても source / modification の名前は変わらない。
#[test]
fn ara_トラックの片は_modification_を共有し_region_は分割前の位置を切り分ける() {
    use common::protocol::AraClipSpec;
    let (mut app, mut rx) = audio_app_with_plugin_rx();
    let ara_id = "test.melodyne";
    app.ipc.plugin_db = Some(std::sync::Arc::new(common::plugin_db::PluginDatabase::new(
        vec![common::plugin_db::PluginEntry {
            id: ara_id.into(),
            format: common::plugin_format::PluginFormat::Vst3,
            name: "Test ARA".into(),
            vendor: "Test".into(),
            version: "1.0".into(),
            features: vec![common::plugin_db::CLAP_FEATURE_ARA_SUPPORTED.into(), "audio-effect".into()],
            path: "C:/fake/ara.vst3".into(),
            descriptor_index: 0,
            has_note_input: false,
            has_note_output: false,
            has_audio_output: true,
            has_audio_input: true,
            has_video_input: false,
            has_video_output: false,
            has_embedded_gui: true,
        }],
        None,
        0,
    )));
    app.edit_song(|song| {
        song.tracks[0].devices.push(Device::Plugin(PluginInstance {
            id: 901,
            ..PluginInstance::new(ara_id.into(), common::plugin_format::PluginFormat::Vst3)
        }));
    });
    // 最後に送った document (作り直し) / region の置き直しを、今の region の一覧に畳む。
    let mut doc: Vec<AraClipSpec> = Vec::new();
    let mut sync = |app: &mut AppData, doc: &mut Vec<AraClipSpec>| {
        app.flush_song_sync();
        for msg in drain(&mut rx) {
            match msg {
                PluginCommand::SetupAraDocument { clips, .. } => *doc = clips,
                PluginCommand::UpdateAraRegions { regions, .. } => {
                    for u in regions {
                        let spec = doc.iter_mut().find(|c| c.region_key == u.region_key).expect("既知の region");
                        spec.placement = u.placement;
                    }
                }
                _ => {}
            }
        }
    };
    sync(&mut app, &mut doc);
    assert_eq!(doc.len(), 1, "前提: 分割前は region 1 つ");
    let whole = doc[0].clone();

    app.cur.view.arrange_snap_choice = GRID_2_BEATS;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.arrangement_hover_clip = Some(CLIP);
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Split { surface: SplitSurface::Clips, at: SplitAt::Grid }));
    app.cur.peph.arrangement_hover_clip = None;
    sync(&mut app, &mut doc);
    assert_eq!(doc.len(), 4, "窓に見えている片ごとに region (隠れた片の region を 2 重に作らない)");
    doc.sort_by(|a, b| a.placement.start_in_playback_seconds.total_cmp(&b.placement.start_in_playback_seconds));
    assert!(doc.iter().all(|c| c.modification_id == whole.modification_id && c.source_id == whole.source_id), "片は同じ modification を共有する");
    let keys: std::collections::HashSet<&str> = doc.iter().map(|c| c.region_key.as_str()).collect();
    assert_eq!(keys.len(), 4, "region のキーは片ごとに別");
    // 再生位置も modification の中の位置も、分割前の region を隙間なく切り分ける (Melodyne の編集の位置がずれない)。
    let (w, first, last) = (whole.placement, doc[0].placement, doc[3].placement);
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(close(first.start_in_playback_seconds, w.start_in_playback_seconds));
    assert!(close(first.start_in_modification_seconds, w.start_in_modification_seconds));
    for pair in doc.windows(2) {
        let (a, b) = (pair[0].placement, pair[1].placement);
        assert!(close(a.start_in_playback_seconds + a.duration_in_playback_seconds, b.start_in_playback_seconds));
        assert!(close(a.start_in_modification_seconds + a.duration_in_modification_seconds, b.start_in_modification_seconds));
    }
    assert!(close(last.start_in_playback_seconds + last.duration_in_playback_seconds, w.start_in_playback_seconds + w.duration_in_playback_seconds));
    assert!(close(
        last.start_in_modification_seconds + last.duration_in_modification_seconds,
        w.start_in_modification_seconds + w.duration_in_modification_seconds
    ));
    assert_eq!(common::ara_ids::source_id(1), whole.source_id, "素材ごとの安定 id");

    // 2 つ目の片だけを Make Unique → その片は別の modification になり、元の modification の編集を写して始める
    // (variation)。 残りの片は元の modification のまま。
    let second = sorted_clip_keys(&app)[1];
    app.handle_event(AppEvent::SelectClip { target: second, additive: false });
    app.handle_event(AppEvent::MakeClipUnique(second));
    sync(&mut app, &mut doc);
    let shown_event = audio_events(&app, second)[1].id;
    let unique = common::ara_ids::region_key(second.clip_id, shown_event);
    let piece = doc.iter().find(|c| c.region_key == unique).expect("独立した片の region");
    assert_ne!(piece.modification_id, whole.modification_id, "独立した片は別の modification");
    let origin = piece.modification_origins.first().expect("写した元");
    assert_eq!((origin.project, origin.modification_id.as_str()), (Some(app.pk()), whole.modification_id.as_str()), "元の編集を写して始める");
    assert_eq!(doc.iter().filter(|c| c.modification_id == whole.modification_id).count(), 3, "残りの片は共有のまま");
}

/// トラック 1 のクリップの key を開始拍順に。
fn sorted_clip_keys(app: &AppData) -> Vec<ClipKey> {
    let mut clips: Vec<&Clip> = app.cur.song_doc.song().tracks[0].clips.iter().collect();
    clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    clips.iter().map(|c| ClipKey { track_id: TRACK, clip_id: c.id }).collect()
}

/// トラック 1 に、48 kHz / 4 秒 (= 120 BPM で 8 拍) の素材を 8 拍の Repitch event で置いたクリップ 1 つ。
fn audio_app() -> AppData {
    audio_app_with_plugin_rx().0
}

/// [`audio_app`] と、plugin host へ送ったコマンドの受け口。
fn audio_app_with_plugin_rx() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, _audio, plugin_rx, _d) = build_app();
    app.edit_song(|song| {
        song.media.audio_sources.insert(
            1,
            common::model::AudioSource {
                path: common::model::AudioSourcePath::Absolute("C:/split-fidelity/b.wav".into()),
                sample_rate: 48_000,
                channels: 1,
                frames: 240_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let event = AudioEvent {
            id: 1,
            source_id: 1,
            event_length_beats: 8.0,
            source_start_frames: 24_000,
            source_end_frames: 216_000,
            stretch_mode: StretchMode::Repitch,
            ..AudioEvent::default()
        };
        let content_id =
            song.alloc_content(ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }), String::new());
        song.tracks[0].clips = vec![Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id, ..Clip::default() }];
        song.tracks[0].next_clip_id = 100;
    });
    (app, plugin_rx)
}

/// クリップ `key` の content の audio event (content の並び)。
fn audio_events(app: &AppData, key: ClipKey) -> Vec<AudioEvent> {
    let song = app.cur.song_doc.song();
    let clip = song.clip_by_key(key).expect("clip");
    song.clip_contents[&clip.content_id].audio_events().expect("audio").to_vec()
}

/// 片の窓 / fade / take と中身が浮動小数の丸めを除いて同じか。
fn assert_same_audio(label: &str, got: &[AudioEvent], want: &[AudioEvent]) {
    use common::model::TimedEvent;
    assert_eq!(got.len(), want.len(), "{label}: 片の数");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        let (gf, wf) = (g.fade(), w.fade());
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9;
        assert!(
            close(gf.start_in_clip_beats, wf.start_in_clip_beats)
                && close(gf.len_beats, wf.len_beats)
                && close(gf.fade_in_beats, wf.fade_in_beats)
                && close(gf.fade_out_beats, wf.fade_out_beats)
                && close(gf.fade_in_lead_beats, wf.fade_in_lead_beats)
                && close(gf.fade_out_trail_beats, wf.fade_out_trail_beats)
                && close(g.take_head_beats, w.take_head_beats)
                && close(g.take_tail_beats, w.take_tail_beats),
            "{label}: 片 {i}\n got  {g:?}\n want {w:?}"
        );
        assert_eq!(g.material(), w.material(), "{label}: 片 {i} の中身");
    }
}

/// オーディオエディタで唯一のクリップの event を 2 拍ごとに割る。
fn split_events_in_audio_editor(app: &mut AppData) {
    app.handle_event(AppEvent::OpenAudioEditor(CLIP));
    app.cur.view.arrange_snap_choice = GRID_2_BEATS;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.audio_editor_hover_beat_in_clip = Some(1.0);
    app.cur.peph.audio_editor_zoom_x = 100.0;
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Split { surface: SplitSurface::Clips, at: SplitAt::Grid }));
    app.handle_event(AppEvent::CloseAudioEditor);
}

/// 1 つのクリップの中で割った片 (窓の中でひと続き) に Inspector の fade / 逆再生 / gain を掛けると、分割前に
/// 同じ編集を掛けてから割ったのと同じ片になる (fade は外側の端、逆再生はひと続き全体)。 旧実装は片ごとに
/// 掛けていたので、切り口ごとに fade が付き、片ごとに逆向きに読んだ。
#[test]
fn ひと続きの片への_inspector_編集は分割前に掛けてから割ったのと同じ() {
    let edit = |app: &mut AppData| {
        app.handle_event(AppEvent::SetClipFadeInBeats { target: CLIP, beats: 3.0 });
        app.handle_event(AppEvent::SetClipFadeOutBeats { target: CLIP, beats: 1.5 });
        app.handle_event(AppEvent::SetClipReversed { target: CLIP, reversed: true });
        app.handle_event(AppEvent::SetClipGainDb { target: CLIP, gain_db: -4.0 });
    };
    let mut before = audio_app();
    edit(&mut before);
    split_events_in_audio_editor(&mut before);

    let mut after = audio_app();
    split_events_in_audio_editor(&mut after);
    assert_eq!(audio_events(&after, CLIP).len(), 4, "前提: 2 拍ごとに 4 片");
    edit(&mut after);
    assert_same_audio("分割後に掛けた片", &audio_events(&after, CLIP), &audio_events(&before, CLIP));

    // Inspector の表示もひと続きから読む (fade の長さはひと続きの端のランプ、上限はひと続きの長さ)。
    after.handle_event(AppEvent::SelectClip { target: CLIP, additive: false });
    let summary = after.inspector_audio_event_summary().expect("audio clip");
    assert_eq!((summary.fade_in_beats, summary.fade_out_beats, summary.fade_max_beats), (3.0, 1.5, 8.0));
}

/// クリップごと割った片 (content を共有して別の窓を見る) へのクリップ単位の編集は、そのクリップの窓の片にだけ
/// 効き、反対側のクリップの片は変わらない。 Auto-Crossfade は窓の端の片の続きを鳴らし合う形で掛かる
/// (content の末尾の event = 別のクリップの片には掛けない)。
#[test]
fn 分割したクリップへの編集と_auto_fade_crossfade_は反対側のクリップに効かない() {
    let mut app = audio_app();
    app.cur.view.arrange_snap_choice = GRID_2_BEATS;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.arrangement_hover_clip = Some(CLIP);
    app.handle_event(AppEvent::SplitJoin(SplitJoinEvent::Split { surface: SplitSurface::Clips, at: SplitAt::Grid }));
    app.cur.peph.arrangement_hover_clip = None;
    let keys = sorted_clip_keys(&app);
    assert_eq!(keys.len(), 4, "前提: 4 クリップ");
    let split = audio_events(&app, keys[0]);
    assert_eq!(split.len(), 4, "前提: 4 クリップが 1 つの content の 4 片を見る");
    let piece = |app: &AppData, i: usize| audio_events(app, keys[i])[i].clone();

    // 2 つ目のクリップの gain / 移調だけ。
    app.handle_event(AppEvent::SetClipGainDb { target: keys[1], gain_db: -9.0 });
    app.handle_event(AppEvent::SetClipPitchSemitones { target: keys[1], semitones: 5.0 });
    let now = audio_events(&app, keys[0]);
    assert_eq!((now[1].gain_db, now[1].pitch_semitones), (-9.0, 5.0));
    for i in [0, 2, 3] {
        assert_eq!(now[i], split[i], "反対側のクリップの片 {i} は変わらない");
    }

    // 3 つ目のクリップだけ Auto-Fade → その片の窓の端にだけ fade。
    app.handle_event(AppEvent::SelectClip { target: keys[2], additive: false });
    app.handle_event(AppEvent::AutoFadeSelectedClips);
    let faded = piece(&app, 2);
    assert!(faded.fade_in_beats > 0.0 && faded.fade_out_beats > 0.0 && faded.fade_in_lead_beats == 0.0);
    assert_eq!(piece(&app, 3), split[3], "Auto-Fade は隣のクリップの片に効かない");

    // 1 つ目と 2 つ目の境界 (拍 2) に Auto-Crossfade → 前の片の尻と次の片の頭に同じ区間のランプ。
    app.handle_event(AppEvent::SelectClip { target: keys[0], additive: false });
    app.handle_event(AppEvent::SelectClip { target: keys[1], additive: true });
    let steps = app.cur.song_doc.history_labels().len();
    app.handle_event(AppEvent::AutoCrossfadeSelectedClips);
    let (a, b) = (piece(&app, 0), piece(&app, 1));
    let song = app.cur.song_doc.song();
    let tail = song.clip_by_key(keys[0]).expect("a").xfade_tail_beats;
    let lead = song.clip_by_key(keys[1]).expect("b").xfade_lead_beats;
    assert!(tail > 0.0 && lead > 0.0, "両側に張り出す: {}", app.ui_ephemeral.status_message);
    assert_eq!((a.fade_out_beats, a.fade_out_trail_beats), (tail + lead, tail), "前の片のランプは境界の先で終わる");
    assert_eq!((b.fade_in_beats, b.fade_in_lead_beats), (tail + lead, lead), "次の片のランプは境界の手前で始まる");
    assert_eq!(piece(&app, 3), split[3], "content の末尾の片 (4 つ目のクリップ) には掛からない");
    assert_eq!(app.cur.song_doc.history_labels().len(), steps + 1, "Auto-Crossfade は 1 undo step");
}
