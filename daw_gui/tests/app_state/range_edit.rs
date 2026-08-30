//! `docs/plan_range_selection.md` の回帰テスト: **範囲操作**。
//!
//! 範囲操作は「範囲の境界で分割し、範囲部分だけに適用する」 で統一されている。
//! ここで押さえるのは、その規則が **クリップの窓** に正しく落ちること —
//! Delete で両端が残る / `J` の結果が範囲そのものになる / ミュートが範囲部分だけに
//! 効く / 範囲がクリップ選択と往復する。

use common::model::{Clip, ClipContent, ClipKey, LaneRef, MidiContent, Note};

use daw_gui::app::{AppData, AppEvent};

use super::support;

/// `support::build_app` の既定トラック id。
const TRACK: u32 = 1;

fn key(clip_id: u32) -> ClipKey {
    ClipKey { track_id: TRACK, clip_id }
}

/// `(id, start, len)` のクリップを並べたトラックを作る。 中身は 1 拍ごとの
/// C4 ノート (窓を切ったときに「何が残ったか」が数えられる)。
fn app_with_clips(specs: &[(u32, f64, f64)]) -> AppData {
    let (mut app, _a, _p, _d) = support::build_app();
    let specs = specs.to_vec();
    app.edit_song(move |song| {
        song.tracks[0].clips.clear();
        song.tracks[0].next_clip_id = 100;
        for (id, start, len) in specs {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = len.ceil().max(1.0) as usize;
            let notes: Vec<Note> = (0..n)
                .map(|i| Note {
                    id: i as u32 + 1,
                    #[allow(clippy::cast_precision_loss)]
                    start_beat: i as f64,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                })
                .collect();
            let cid = song.alloc_content(
                ClipContent::Midi(MidiContent { next_note_id: notes.len() as u32 + 1, notes }),
                String::new(),
            );
            song.tracks[0].clips.push(Clip {
                id,
                start_beat: start,
                length_beats: len,
                content_id: cid,
                ..Clip::default()
            });
        }
    });
    app
}

/// `(start, len)` を開始拍順で。
fn spans(app: &AppData) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = app.song_doc.song().tracks[0]
        .clips
        .iter()
        .map(|c| (c.start_beat, c.length_beats))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v
}

fn set_range(app: &mut AppData, start: f64, end: f64) {
    app.handle_event(AppEvent::SetTimeSelection {
        start_beat: start,
        end_beat: end,
        lanes: vec![LaneRef::Track(TRACK)],
    });
}

#[test]
fn 範囲の_delete_は境界で分割して範囲部分だけ消す() {
    // A [0,8) と B [8,16) に 4〜12 の範囲 → 0-4 と 12-16 が残る。
    let mut app = app_with_clips(&[(1, 0.0, 8.0), (2, 8.0, 8.0)]);
    set_range(&mut app, 4.0, 12.0);
    app.delete_current_surface(false);
    assert_eq!(spans(&app), vec![(0.0, 4.0), (12.0, 4.0)]);
}

#[test]
fn 範囲の_delete_はクリップの真ん中を抜いて両端を残す() {
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    set_range(&mut app, 4.0, 12.0);
    app.delete_current_surface(false);
    assert_eq!(spans(&app), vec![(0.0, 4.0), (12.0, 4.0)]);
}

#[test]
fn 範囲の_j_は結果クリップが範囲そのものになる() {
    // 素材は 6〜10 にしか無いが、範囲 4〜12 で結合すると前後に空白が付く。
    let mut app = app_with_clips(&[(1, 6.0, 4.0)]);
    set_range(&mut app, 4.0, 12.0);
    app.handle_event(AppEvent::GlueSelectedClips);
    assert_eq!(spans(&app), vec![(4.0, 8.0)], "結果クリップ = 範囲そのもの");
    // 素材は content の 2〜6 拍目 (= song 6〜10) に居る。
    let song = app.song_doc.song();
    let clip = &song.tracks[0].clips[0];
    let notes = song.clip_notes(clip);
    let first = notes.iter().map(|n| n.start_beat).fold(f64::INFINITY, f64::min);
    assert!((first - 2.0).abs() < 1e-9, "先頭に 2 拍の空白が付く: got {first}");
}

#[test]
fn 範囲の_j_ははみ出した部分を分割して残す() {
    // A [0,16) に範囲 4〜12 → 0-4 / 4-12 (結合) / 12-16 の 3 つ。
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    set_range(&mut app, 4.0, 12.0);
    app.handle_event(AppEvent::GlueSelectedClips);
    assert_eq!(spans(&app), vec![(0.0, 4.0), (4.0, 8.0), (12.0, 4.0)]);
}

#[test]
fn 範囲のミュートは範囲部分だけを消音する() {
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    set_range(&mut app, 4.0, 12.0);
    app.apply_mute_time_selection();
    let song = app.song_doc.song();
    let mut got: Vec<(f64, bool)> =
        song.tracks[0].clips.iter().map(|c| (c.start_beat, c.muted)).collect();
    got.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(got, vec![(0.0, false), (4.0, true), (12.0, false)]);
}

#[test]
fn クリップを選ぶと範囲がその占有区間になる() {
    let mut app = app_with_clips(&[(1, 4.0, 8.0)]);
    app.handle_event(AppEvent::SelectClip { target: key(1), additive: false });
    let sel = app.selection.time.as_ref().expect("範囲が立つ");
    assert_eq!((sel.start_beat, sel.end_beat), (4.0, 12.0));
    assert_eq!(app.selected_clip_refs(), vec![key(1)]);
}

#[test]
fn ctrl_クリックで離れた_2_クリップを拾うと間のクリップも入る() {
    // Live 実機と同じ — 選択は単一の連続区間なので、間のクリップも範囲に入る。
    let mut app = app_with_clips(&[(1, 0.0, 4.0), (2, 4.0, 4.0), (3, 8.0, 4.0)]);
    app.handle_event(AppEvent::SelectClip { target: key(1), additive: false });
    app.handle_event(AppEvent::SelectClip { target: key(3), additive: true });
    let sel = app.selection.time.as_ref().expect("範囲が立つ");
    assert_eq!((sel.start_beat, sel.end_beat), (0.0, 12.0));
    assert_eq!(app.selected_clip_refs(), vec![key(1), key(2), key(3)]);
}

#[test]
fn 範囲の_copy_は前後の空白ごと運ぶ() {
    // 範囲 4〜12 で素材は 6〜10。貼った先でも先頭 2 拍の空白が保たれる。
    let mut app = app_with_clips(&[(1, 6.0, 4.0)]);
    set_range(&mut app, 4.0, 12.0);
    let (json, count) = app.copy_time_selection_clip().expect("コピーできる");
    assert_eq!(count, 1);
    assert!(json.contains("\"start_beat\":2.0"), "範囲の先頭からの相対位置で運ぶ: {json}");
}

#[test]
fn アレンジで引いた範囲がピアノロールの表示クリップになる() {
    let mut app = app_with_clips(&[(1, 0.0, 8.0), (2, 8.0, 8.0)]);
    set_range(&mut app, 4.0, 12.0);
    assert_eq!(
        app.selected_clip_refs(),
        vec![key(1), key(2)],
        "範囲と交差するクリップが選択される"
    );
    assert_eq!(
        app.shown_pianoroll_clips(),
        vec![key(1), key(2)],
        "その MIDI クリップがピアノロールに出る"
    );
    assert_eq!(app.pianoroll_target_clip(), Some(key(1)), "対象は範囲の先頭に近い方");
}

#[test]
fn 矢印キーの範囲ナッジは中身も範囲も一緒に動く() {
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    set_range(&mut app, 4.0, 8.0);
    // 矢印キー (←→) の経路。 素材を動かすのはこの 1 本だけ。
    app.nudge_time_selection(4.0);
    // 4〜8 の中身が 8〜12 へ移り、元の場所は空く。
    assert_eq!(spans(&app), vec![(0.0, 4.0), (8.0, 4.0), (12.0, 4.0)]);
    let sel = app.selection.time.as_ref().expect("範囲は残る");
    assert_eq!((sel.start_beat, sel.end_beat), (8.0, 12.0), "範囲も一緒に動く");
}

#[test]
fn 範囲ナッジは曲頭より前へ行かない() {
    let mut app = app_with_clips(&[(1, 0.0, 8.0)]);
    set_range(&mut app, 0.0, 4.0);
    app.nudge_time_selection(-4.0);
    let sel = app.selection.time.as_ref().expect("範囲は残る");
    assert_eq!((sel.start_beat, sel.end_beat), (0.0, 4.0), "曲頭で止まる");
}

#[test]
fn 選択外のクリップのヘッダを掴んだら_その区間だけが動いて選択になる() {
    // クリップヘッダのドラッグは `move_time_range` 1 本 (矢印キーのナッジと同じ口)。
    // 掴んだクリップに範囲が掛かっていなければ、その占有区間が範囲になる。
    let mut app = app_with_clips(&[(1, 0.0, 4.0), (2, 8.0, 4.0)]);
    set_range(&mut app, 0.0, 4.0);
    app.move_time_range(8.0, 12.0, 4.0, &[(TRACK, TRACK)]);
    assert_eq!(spans(&app), vec![(0.0, 4.0), (12.0, 4.0)]);
    let sel = app.selection.time.as_ref().expect("範囲が立つ");
    assert_eq!((sel.start_beat, sel.end_beat), (12.0, 16.0), "動かした先が新しい選択");
}

#[test]
fn 範囲の複製は元を残して窓を詰めて置く() {
    // Ctrl+ドラッグ。 元クリップは 1 拍も割らず、複製だけが範囲の窓を持つ。
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    set_range(&mut app, 4.0, 8.0);
    app.copy_time_range(4.0, 8.0, 8.0, &[(TRACK, TRACK)], false);
    // 行き先 [12,16) は上書き規則で元クリップから削り取られる。
    assert_eq!(spans(&app), vec![(0.0, 12.0), (12.0, 4.0)]);
    let song = app.song_doc.song();
    let copy = song.tracks[0].clips.iter().find(|c| c.start_beat == 12.0).expect("複製がある");
    assert!(
        (copy.content_offset_beats - 4.0).abs() < 1e-9,
        "範囲の頭 (content の 4 拍目) から見せる: got {}",
        copy.content_offset_beats
    );
}

#[test]
fn ピアノロールの_d_は範囲の長さぶん送る() {
    // 裏拍 1 音だけのパターン。 外接 span (1 拍) ではなく**範囲の長さ** 4 拍ぶん送る
    // — 外接で送るとグリッドから外れて、実機で「複製できない」になる。
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    let clip = key(1);
    app.edit_song(|song| {
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(common::model::ClipContent::Midi(midi)) = song.clip_contents.get_mut(&cid) {
            midi.notes = vec![Note {
                id: 1,
                start_beat: 1.5,
                duration_beats: 0.5,
                pitch: 60,
                velocity: 100,
                lyric: None,
                muted: false,
            }];
            midi.next_note_id = 2;
        }
    });
    app.handle_event(AppEvent::SetTimeSelection {
        start_beat: 0.0,
        end_beat: 4.0,
        lanes: vec![LaneRef::KeyTrack { clip, pitch: 60 }],
    });
    app.handle_event(AppEvent::DuplicateSelectedNotes);
    let song = app.song_doc.song();
    let mut starts: Vec<f64> =
        song.clip_notes(&song.tracks[0].clips[0]).iter().map(|n| n.start_beat).collect();
    starts.sort_by(f64::total_cmp);
    assert_eq!(starts, vec![1.5, 5.5], "裏拍の位置が保たれる");
    let sel = app.selection.time.as_ref().expect("範囲は残る");
    assert_eq!((sel.start_beat, sel.end_beat), (4.0, 8.0), "範囲も 1 つ後ろへ送る");
}

#[test]
fn 範囲の複製を連打しても行き先に居たクリップを巻き込まない() {
    // A [0,4) を選び、D 相当を 2 回。 行き先 [4,8) には別クリップ B が居る。
    // 1 回目で B は上書き規則で消え、範囲 [4,8) には複製しか居ない。 2 回目は
    // その 1 本だけを複製する — 旧実装は B ごと選択に巻き込んで雪だるまになった。
    let mut app = app_with_clips(&[(1, 0.0, 4.0), (2, 4.0, 4.0)]);
    app.handle_event(AppEvent::SelectClip { target: key(1), additive: false });
    for _ in 0..2 {
        let sel = app.selection.time.as_ref().expect("範囲が立つ").clone();
        app.copy_time_range(
            sel.start_beat,
            sel.end_beat,
            sel.len_beats(),
            &[(TRACK, TRACK)],
            false,
        );
    }
    assert_eq!(spans(&app), vec![(0.0, 4.0), (4.0, 4.0), (8.0, 4.0)]);
    let sel = app.selection.time.as_ref().expect("範囲が立つ");
    assert_eq!((sel.start_beat, sel.end_beat), (8.0, 12.0));
    assert_eq!(app.selected_clip_refs().len(), 1, "選択は複製 1 本だけ");
}

#[test]
fn ピアノロールの複製は行き先のノートを上書きする() {
    // 範囲 [0,4) に 1.5 拍の音、行き先 [4,8) には 5.0 拍に別の音 (同じ鍵盤行)。
    // 複製後、行き先に残るのは複製した 5.5 拍だけ。
    let mut app = app_with_clips(&[(1, 0.0, 16.0)]);
    let clip = key(1);
    app.edit_song(|song| {
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(common::model::ClipContent::Midi(midi)) = song.clip_contents.get_mut(&cid) {
            midi.notes = vec![
                Note { id: 1, start_beat: 1.5, duration_beats: 0.5, pitch: 60, velocity: 100, lyric: None, muted: false },
                Note { id: 2, start_beat: 5.0, duration_beats: 0.5, pitch: 60, velocity: 100, lyric: None, muted: false },
            ];
            midi.next_note_id = 3;
        }
    });
    app.handle_event(AppEvent::SetTimeSelection {
        start_beat: 0.0,
        end_beat: 4.0,
        lanes: vec![LaneRef::KeyTrack { clip, pitch: 60 }],
    });
    app.handle_event(AppEvent::DuplicateSelectedNotes);
    let song = app.song_doc.song();
    let mut starts: Vec<f64> =
        song.clip_notes(&song.tracks[0].clips[0]).iter().map(|n| n.start_beat).collect();
    starts.sort_by(f64::total_cmp);
    assert_eq!(starts, vec![1.5, 5.5], "行き先に元から居た 5.0 は消える");
}
