//! r.md #132 の回帰テスト: **`Shift+E` でグリッド単位に分割** と、同じ根で直した `E` / `J`
//! (`docs/plan_rmd_132_grid_split.md`)。
//!
//! どれも `AppData::handle_event` (コマンド層) で完結するので widget を回さずに検証する。
//! キー → `AppEvent::SplitJoin` の配線は `view::root` の unit test が担当。

use common::model::{AudioContent, AudioEvent, Clip, ClipContent, ClipKey, MidiContent, Note};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::event_split::{SplitAt, SplitJoinEvent, SplitSurface};

use super::support;

/// 1 event の観測値: (開始拍, 長さ拍, source 先頭 frame, source 末尾 frame, take 頭, take 尻, fade-in, fade-out)。
type EventShape = (f64, f64, u64, u64, f64, f64, f64, f64);

/// `support::build_app` の既定トラックの id。
const TRACK: u32 = 1;
const A: ClipKey = ClipKey { track_id: TRACK, clip_id: 1 };
const B: ClipKey = ClipKey { track_id: TRACK, clip_id: 2 };

/// ピアノロール / アレンジのスナップの選択肢 (`view::snap::SNAP_LABELS` の index)。
const GRID_1_4: u8 = 1; // 1 拍
const GRID_1_2: u8 = 0; // 2 拍
const GRID_1_128: u8 = 6; // 1/32 拍

fn split(surface: SplitSurface, at: SplitAt) -> AppEvent {
    AppEvent::SplitJoin(SplitJoinEvent::Split { surface, at })
}

fn note(id: u32, start: f64, len: f64, pitch: u8, lyric: Option<&str>) -> Note {
    Note {
        id,
        start_beat: start,
        duration_beats: len,
        pitch,
        velocity: 90,
        lyric: lyric.map(str::to_string),
        muted: false,
    }
}

/// トラック 1 本に `clips` = (id, start, len) の MIDI クリップを並べる。 全クリップが同じ
/// content (`notes`) を見る (2 本以上なら linked clip)。
fn app_with_midi(clips: &[(u32, f64, f64)], notes: Vec<Note>) -> AppData {
    let (mut app, _a, _p, _d) = support::build_app();
    let clips = clips.to_vec();
    app.edit_song(move |song| {
        let next_note_id = notes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        let cid = song.alloc_content(
            ClipContent::Midi(MidiContent { notes, next_note_id }),
            String::new(),
        );
        song.tracks[0].clips = clips
            .into_iter()
            .map(|(id, start, len)| Clip {
                id,
                start_beat: start,
                length_beats: len,
                content_id: cid,
                ..Clip::default()
            })
            .collect();
        song.tracks[0].next_clip_id = 100;
    });
    app
}

/// content のノートを `(start, len, pitch, lyric)` で開始拍順に。
fn notes_of(app: &AppData, key: ClipKey) -> Vec<(f64, f64, u8, Option<String>)> {
    let song = app.cur.song_doc.song();
    let clip = song.clip_by_key(key).expect("clip");
    let mut out: Vec<_> = song
        .clip_notes(clip)
        .iter()
        .map(|n| (n.start_beat, n.duration_beats, n.pitch, n.lyric.clone()))
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));
    out
}

fn ids_of(app: &AppData, key: ClipKey) -> Vec<u32> {
    let song = app.cur.song_doc.song();
    let clip = song.clip_by_key(key).expect("clip");
    let mut notes = song.clip_notes(clip).to_vec();
    notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    notes.iter().map(|n| n.id).collect()
}

fn piano_grid(app: &mut AppData, choice: u8, snap: bool) {
    app.cur.view.pianoroll_snap_choice = choice;
    app.cur.view.pianoroll_snap_enabled = snap;
}

fn last_label(app: &AppData) -> &'static str {
    let doc = &app.cur.song_doc;
    doc.history_labels()[doc.history_current()]
}

fn ly(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// 1/4 グリッドで 1.5〜3.5 拍のノート → 1.5-2 / 2-3 / 3-3.5 の 3 片。 先頭が元の id と歌詞、
/// 後ろは「ー」(音節を歌い直さず伸ばす)。 1 回の undo で元に戻る。 スナップを切っていても
/// 選んでいる分割で割る。
#[test]
fn ピアノロールの_shift_e_はグリッド線ごとにノートを割り_後ろの片の歌詞は長音になる() {
    let mut app = app_with_midi(&[(1, 0.0, 8.0)], vec![note(7, 1.5, 2.0, 60, Some("あ"))]);
    app.handle_event(AppEvent::SetClipSelection(vec![A]));
    piano_grid(&mut app, GRID_1_4, false);
    let before = app.cur.song_doc.history_current();

    app.handle_event(split(SplitSurface::Notes, SplitAt::Grid));

    assert_eq!(
        notes_of(&app, A),
        vec![(1.5, 0.5, 60, ly("あ")), (2.0, 1.0, 60, ly("ー")), (3.0, 0.5, 60, ly("ー"))]
    );
    let ids = ids_of(&app, A);
    assert_eq!(ids[0], 7, "先頭の片が元の安定 id を持つ");
    assert!(ids[1] != 7 && ids[2] != 7 && ids[1] != ids[2], "後ろの片は新しい id: {ids:?}");
    assert_eq!(app.cur.song_doc.history_current() - before, 1, "1 操作 = 1 undo step");
    assert_eq!(last_label(&app), "ノートをグリッドで分割");

    app.cur.song_doc.undo();
    assert_eq!(notes_of(&app, A), vec![(1.5, 2.0, 60, ly("あ"))], "1 回の undo で元に戻る");
}

/// 最短ノート長 (1/16 拍) より短い片はできず、隣の片にくっつく。
#[test]
fn グリッド分割は最短ノート長より短い片を作らない() {
    // グリッド線 (1 拍) がノートの頭から 1/32 拍の位置 → 頭の 1/32 拍は次の片へ。
    let mut app = app_with_midi(&[(1, 0.0, 8.0)], vec![note(1, 1.0 - 1.0 / 32.0, 2.0 + 1.0 / 32.0, 60, None)]);
    app.handle_event(AppEvent::SetClipSelection(vec![A]));
    piano_grid(&mut app, GRID_1_4, true);
    app.handle_event(split(SplitSurface::Notes, SplitAt::Grid));
    assert_eq!(notes_of(&app, A), vec![(1.0 - 1.0 / 32.0, 1.0 + 1.0 / 32.0, 60, None), (2.0, 1.0, 60, None)]);

    // グリッド (1/32 拍) が最短長より細かい → 1/16 拍ずつの片。 歌詞の無いノートは無いまま。
    let mut app = app_with_midi(&[(1, 0.0, 8.0)], vec![note(1, 0.0, 0.25, 60, None)]);
    app.handle_event(AppEvent::SetClipSelection(vec![A]));
    piano_grid(&mut app, GRID_1_128, true);
    app.handle_event(split(SplitSurface::Notes, SplitAt::Grid));
    let spans: Vec<(f64, f64)> = notes_of(&app, A).iter().map(|n| (n.0, n.1)).collect();
    assert_eq!(spans, vec![(0.0, 0.0625), (0.0625, 0.0625), (0.125, 0.0625), (0.1875, 0.0625)]);
}

/// `E` の回帰: 後ろの片の歌詞は「ー」(以前は `None` = 実際には「ら」と歌われた)、
/// 1 回の `E` が 1 undo step で、履歴に操作名が付く。
#[test]
fn ピアノロールの_e_は後ろの片を長音にして_1_undo_step() {
    let mut app = app_with_midi(&[(1, 0.0, 8.0)], vec![note(1, 0.0, 2.0, 62, Some("か"))]);
    app.handle_event(AppEvent::SetClipSelection(vec![A]));
    piano_grid(&mut app, 3, true); // 1/16
    app.cur.peph.pianoroll_hover_beat_song_raw = Some(1.01);
    let before = app.cur.song_doc.history_current();

    app.handle_event(split(SplitSurface::Notes, SplitAt::Cursor { snap: true }));

    assert_eq!(notes_of(&app, A), vec![(0.0, 1.0, 62, ly("か")), (1.0, 1.0, 62, ly("ー"))]);
    assert_eq!(app.cur.song_doc.history_current() - before, 1);
    assert_eq!(last_label(&app), "ノート分割");
}

/// 同じ content を linked clip 2 本で同時に表示し、両方の複製を選んでいても、編集は content
/// ごとに 1 回だけ。 以前は slot ごとに走り、1 回目で index がずれた後に 2 回目が別のノートを
/// 消していた (Delete) / 切っていた (E)。
#[test]
fn linked_clip_を同時に表示しても編集は_content_ごとに_1_回() {
    let notes = vec![note(1, 0.0, 1.0, 60, None), note(2, 1.0, 1.0, 62, None), note(3, 2.0, 1.0, 64, None)];
    let mut app = app_with_midi(&[(1, 0.0, 4.0), (2, 4.0, 4.0)], notes);
    app.handle_event(AppEvent::SetClipSelection(vec![A, B]));
    assert_eq!(app.shown_pianoroll_clips(), vec![A, B]);

    // 真ん中のノートを両方の複製で選んで Delete → そのノートだけが消える。
    app.handle_event(AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 1), AppData::pack_note_id(1, 1)]));
    assert_eq!(app.selected_note_ids().len(), 2, "両方の複製が選ばれている");
    app.handle_event(AppEvent::DeleteSelectedNotes);
    let pitches: Vec<u8> = notes_of(&app, A).iter().map(|n| n.2).collect();
    assert_eq!(pitches, vec![60, 64], "選んだノートだけが消える");

    // 先頭のノートを両方の複製で選び、B の複製の真ん中 (song 4.5 拍) で E → 1 回だけ切れる。
    app.handle_event(AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 0), AppData::pack_note_id(1, 0)]));
    piano_grid(&mut app, 3, true); // 1/16
    app.cur.peph.pianoroll_hover_beat_song_raw = Some(4.5);
    app.handle_event(split(SplitSurface::Notes, SplitAt::Cursor { snap: true }));
    assert_eq!(notes_of(&app, A), vec![(0.0, 0.5, 60, None), (0.5, 0.5, 60, None), (2.0, 1.0, 64, None)]);
}

/// アレンジの `Shift+E`: ポインタ直下のクリップをアレンジのグリッド線で割る。 片は同じ content を
/// 別の窓で見て、跨いでいたノートは切り口で割れて各片で鳴る。 最短長より短い片は作らず、
/// 片は全部選択される。
#[test]
fn アレンジの_shift_e_はクリップをグリッド線で割る() {
    // クリップは 1/32 拍だけ小節線の手前から始まる → 先頭の 1/32 拍は次の片へくっつく。
    let start = 1.0 - 1.0 / 32.0;
    let mut app = app_with_midi(&[(1, start, 4.0 - start)], vec![note(1, 0.0, 4.0 - start, 60, Some("ら"))]);
    // 両端に隣とのクロスフェードの張り出しが付いている。
    app.edit_song(|song| {
        song.tracks[0].clips[0].xfade_lead_beats = 0.01;
        song.tracks[0].clips[0].xfade_tail_beats = 0.02;
    });
    app.cur.view.arrange_snap_choice = GRID_1_4;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.arrangement_hover_clip = Some(A);

    app.handle_event(split(SplitSurface::Clips, SplitAt::Grid));

    let song = app.cur.song_doc.song();
    let mut clips: Vec<&Clip> = song.tracks[0].clips.iter().collect();
    clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    let spans: Vec<(f64, f64)> = clips.iter().map(|c| (c.start_beat, c.length_beats)).collect();
    assert_eq!(spans, vec![(start, 2.0 - start), (2.0, 1.0), (3.0, 1.0)]);
    assert_eq!(clips[0].id, 1, "先頭の片が元のクリップ");
    assert!(clips.iter().all(|c| c.content_id == clips[0].content_id), "片は同じ content の窓");
    // 片同士は接しているので、切り口の端が張り出しを継ぐと同じ素材が切り口の先で 2 重に鳴る。
    let overhangs: Vec<(f64, f64)> = clips.iter().map(|c| (c.xfade_lead_beats, c.xfade_tail_beats)).collect();
    assert_eq!(overhangs, vec![(0.01, 0.0), (0.0, 0.0), (0.0, 0.02)], "外側の端だけが元の張り出しを持つ");
    // 跨いでいたノートは切り口で割れ、各片の窓に発音開始が入る。
    for c in &clips {
        let (w0, w1) = c.content_window();
        assert!(
            song.clip_notes(c).iter().any(|n| n.start_beat >= w0 - 1e-9 && n.start_beat < w1),
            "窓 {w0}..{w1} で鳴るノートがある"
        );
    }
    let mut selected = app.selected_clip_refs();
    selected.sort_by_key(|k| k.clip_id);
    let mut all: Vec<ClipKey> = clips.iter().map(|c| ClipKey { track_id: TRACK, clip_id: c.id }).collect();
    all.sort_by_key(|k| k.clip_id);
    assert_eq!(selected, all, "片は全部選択される");
    assert_eq!(last_label(&app), "クリップをグリッドで分割");
}

/// オーディオエディタの `Shift+E`: ポインタが乗っている event をアレンジのグリッド線で割る。
/// 片は元の event の take の窓 (source の範囲は全片が継ぎ、take の頭 / 尻で見せる区間を持つ)、
/// 片は全部選択される。
#[test]
fn オーディオエディタの_shift_e_は_event_をグリッド線で割る() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let cid = song.alloc_content(
            ClipContent::Audio(AudioContent {
                events: vec![AudioEvent {
                    id: 1,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    source_start_frames: 0,
                    source_end_frames: 4 * 24_000,
                    fade_in_beats: 0.25,
                    fade_out_beats: 0.5,
                    ..AudioEvent::default()
                }],
                next_event_id: 2,
            }),
            String::new(),
        );
        song.tracks[0].clips =
            vec![Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: cid, ..Clip::default() }];
    });
    app.handle_event(AppEvent::OpenAudioEditor(A));
    app.cur.view.arrange_snap_choice = GRID_1_2;
    app.cur.view.arrange_snap_enabled = true;
    app.cur.peph.audio_editor_hover_beat_in_clip = Some(1.0);
    app.cur.peph.audio_editor_zoom_x = 100.0;

    app.handle_event(split(SplitSurface::Clips, SplitAt::Grid));

    let song = app.cur.song_doc.song();
    let clip = song.clip_by_key(A).expect("clip");
    let Some(ClipContent::Audio(audio)) = song.clip_contents.get(&clip.content_id) else {
        panic!("audio content");
    };
    let got: Vec<EventShape> = audio
        .events
        .iter()
        .map(|e| {
            (
                e.event_start_in_clip_beats,
                e.event_length_beats,
                e.source_start_frames,
                e.source_end_frames,
                e.take_head_beats,
                e.take_tail_beats,
                e.fade_in_beats,
                e.fade_out_beats,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![(0.0, 2.0, 0, 96_000, 0.0, 2.0, 0.25, 0.0), (2.0, 2.0, 0, 96_000, 2.0, 0.0, 0.0, 0.5)],
        "外側の端の fade だけが残り、片は take を隙間なく並べる窓になる"
    );
    assert_eq!(audio.events[0].id, 1, "先頭の片が元の id");
    assert_eq!(app.selected_audio_event_indices(), vec![0, 1], "片は全部選択される");
}
