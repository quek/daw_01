//! r.md #64 (MIDI エディタの編集ロック) + r.md #67 (カーソルキーでノート編集) の回帰テスト。
//!
//! どちらも `AppData::handle_event` (= コマンド層) で完結するので widget を回さずに検証する。
//! キー → shortcut → event の配線は `view::shortcuts` の unit test と実機確認が担当。

use std::sync::Arc;

use common::model::{Clip, ClipContent, ClipKey, ContentId, MidiContent, Note};
use common::protocol::{AudioCommand, PluginCommand};
use common::scale::{Scale, ScaleChange};
use tokio::sync::mpsc;

use daw_gui::app::{track_with, AppData, AppEvent, ClipRef};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event::NudgeStep;

fn build_app() -> AppData {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
    let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    )
}

fn mk_note(pitch: u8, start: f64, len: f64) -> Note {
    Note { pitch, start_beat: start, duration_beats: len, velocity: 100, ..Note::default() }
}

/// track/clip を `specs` のぶんだけ作る (1 track = 1 clip、track_id/clip_id = 10 + i)。
/// 戻り値は各クリップの `ClipKey` (選択に使う)。
fn setup_tracks(app: &mut AppData, specs: &[Vec<Note>]) -> Vec<ClipKey> {
    let specs = specs.to_vec();
    app.edit_song(|song| {
        song.tracks.clear();
        for (i, notes) in specs.into_iter().enumerate() {
            let cid: ContentId = song.alloc_content_id();
            song.clip_contents
                .insert(cid, ClipContent::Midi(MidiContent { notes, ..MidiContent::default() }));
            song.tracks.push(track_with(|t| {
                t.id = 1 + i as u32;
                t.name = format!("T{i}");
                t.clips = vec![Clip {
                    id: 10 + i as u32,
                    content_id: cid,
                    start_beat: 0.0,
                    length_beats: 32.0,
                    ..Clip::default()
                }];
            }));
        }
    });
    (0..app.song_doc.song().tracks.len())
        .map(|i| ClipKey { track_id: 1 + i as u32, clip_id: 10 + i as u32 })
        .collect()
}

fn show_clips(app: &mut AppData, keys: &[ClipKey]) {
    app.selection.selected_clips = keys.to_vec();
    app.selection.selected_clip = keys.last().copied();
}

fn notes_of(app: &AppData, track: usize) -> Vec<Note> {
    let song = app.song_doc.song();
    let clip = &song.tracks[track].clips[0];
    song.clip_notes(clip).to_vec()
}

/// 既定グリッド (1/16 = 0.25 拍) を使うため snap を明示的に有効化する。
fn enable_grid(app: &mut AppData) {
    app.ui_prefs.pianoroll_snap_enabled = true;
    app.ui_prefs.pianoroll_snap_choice = daw_gui::view::snap::CHOICE_PIANOROLL_DEFAULT; // 1/16
}

// ============================================================
// r.md #64 — ロックの効力は「解除 UI が出ている」 ことから導かれる
// ============================================================

/// 詰みの再現と修正: 2 クリップ表示でトラックをロック → そのトラック 1 つだけの表示に絞ると
/// **ロックは効かなくなる** (凡例 = 解除ボタンが出ないため)。
/// 旧実装ではロックが効いたまま解除 UI が消え、開き直す以外に復帰できなかった。
#[test]
fn lock_has_no_effect_while_its_toggle_is_offscreen() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 1.0, 1.0)], vec![mk_note(64, 1.0, 1.0)]]);
    show_clips(&mut app, &keys);
    app.handle_event(AppEvent::TogglePianoRollTrackLock(1)); // track 0 (id=1) をロック

    let t0 = ClipRef { track: 0, clip: 0 };
    assert!(app.is_pianoroll_clip_locked(t0), "2 クリップ表示中はロックが効く (凡例が出ている)");
    assert!(
        !app.all_shown_pianoroll_note_ids().iter().any(|&id| AppData::note_id_clip_slot(id) == 0),
        "ロック中は Ctrl+A の対象外"
    );

    // そのトラックのクリップ 1 つだけを表示 (= 凡例が消える) に絞る。
    show_clips(&mut app, &keys[..1]);
    assert!(
        !app.is_pianoroll_clip_locked(t0),
        "単一表示ではロックの解除 UI が無いので、ロックも効かない (r.md #64)"
    );
    assert_eq!(
        app.all_shown_pianoroll_note_ids(),
        vec![AppData::pack_note_id(0, 0)],
        "Ctrl+A でノートを選べる (= 編集できる)"
    );

    // 複数表示に戻すとロックは元どおり効く (ユーザーの意思は保持されている)。
    show_clips(&mut app, &keys);
    assert!(app.is_pianoroll_clip_locked(t0), "複数表示に戻すとロックが復活する");
}

/// 実効ロック中は既存ノートを動かせない (従来からの効力が壊れていない)。
#[test]
fn locked_track_notes_are_not_moved() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 1.0, 1.0)], vec![mk_note(64, 1.0, 1.0)]]);
    show_clips(&mut app, &keys);
    app.handle_event(AppEvent::TogglePianoRollTrackLock(1));
    app.handle_event(AppEvent::SetNotePositions(vec![(AppData::pack_note_id(0, 0), 9.0, 72)]));
    let n = &notes_of(&app, 0)[0];
    assert!((n.start_beat - 1.0).abs() < 1e-9, "ロック中トラックの note は動かない");
    assert_eq!(n.pitch, 60);
}

/// r.md #64 の 2 点目: **新規ノートを生む経路もロックを見る**。
/// 旧実装は「既存ノートは掴めないのに鉛筆では描ける」 という食い違いを起こしていた。
#[test]
fn locked_track_rejects_new_notes_from_every_path() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![], vec![mk_note(64, 1.0, 1.0)]]);
    show_clips(&mut app, &keys);
    app.handle_event(AppEvent::TogglePianoRollTrackLock(1));

    // (a) 鉛筆 / Insert
    app.handle_event(AppEvent::AddNote {
        track: 0,
        clip: 0,
        start_beat: 2.0,
        duration: 1.0,
        pitch: 60,
    });
    assert!(notes_of(&app, 0).is_empty(), "ロック中トラックには新規ノートを描けない");
    assert!(
        app.ui_ephemeral.status_message.contains("ロック"),
        "拒否した理由がステータスバーに出る: {:?}",
        app.ui_ephemeral.status_message
    );

    // (b) 貼り付け (対象 = anchor クリップ = ロック中トラック)
    app.selection.selected_clip = Some(keys[0]);
    assert_eq!(app.paste_notes_at(vec![mk_note(62, 0.0, 1.0)], 4.0), 0, "貼り付けも拒否される");
    assert!(notes_of(&app, 0).is_empty());

    // ロックを外せば通る (= 拒否がロック起因であることの対)。
    app.handle_event(AppEvent::TogglePianoRollTrackLock(1));
    app.handle_event(AppEvent::AddNote {
        track: 0,
        clip: 0,
        start_beat: 2.0,
        duration: 1.0,
        pitch: 60,
    });
    assert_eq!(notes_of(&app, 0).len(), 1, "解除後は描ける");
}

// ============================================================
// r.md #67 — カーソルキーでノートを移動 / 伸縮 / 音程変更
// ============================================================

/// ←/→ はグリッド 1 つ分の **相対** 移動。届いたリピート回数ぶんまとめて適用する。
#[test]
fn arrow_moves_selection_by_one_grid_step() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 1.0, 1.0)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: 1 });
    assert!((notes_of(&app, 0)[0].start_beat - 1.25).abs() < 1e-9, "1/16 = 0.25 拍 右へ");

    // 1 フレームに 4 回リピートが届いた場合。
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: -4 });
    assert!((notes_of(&app, 0)[0].start_beat - 0.25).abs() < 1e-9, "4 ステップ左 = -1.0 拍");

    // Shift = 1 小節 (4/4 なら 4 拍)。
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Bar, steps: 1 });
    assert!((notes_of(&app, 0)[0].start_beat - 4.25).abs() < 1e-9, "1 小節 = 4 拍 右へ");
}

/// **集合クランプ**: 端に当たった 1 音のために和音が潰れてはいけない。
/// 1 音ずつ 0 で clamp すると団子になり、逆キーを押しても元へ戻らなくなる。
#[test]
fn chord_keeps_its_shape_at_the_left_edge() {
    let mut app = build_app();
    let keys = setup_tracks(
        &mut app,
        &[vec![mk_note(60, 0.0, 1.0), mk_note(64, 0.5, 1.0), mk_note(67, 1.0, 1.0)]],
    );
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes =
        (0..3).map(|i| AppData::pack_note_id(0, i)).collect();

    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Bar, steps: -1 });
    let starts: Vec<f64> = notes_of(&app, 0).iter().map(|n| n.start_beat).collect();
    assert_eq!(starts, vec![0.0, 0.5, 1.0], "左端で止まり、和音の相対位置は保たれる");

    // 右 → 左と往復して元に戻る (可逆性)。per-note clamp だとここで崩れる。
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: 2 });
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: -2 });
    let starts: Vec<f64> = notes_of(&app, 0).iter().map(|n| n.start_beat).collect();
    for (got, want) in starts.iter().zip([0.0, 0.5, 1.0]) {
        assert!((got - want).abs() < 1e-9, "往復で元の配置へ戻る: got {starts:?}");
    }
}

/// ↑/↓ は半音、Shift+↑/↓ は 1 オクターブ。上端でも集合クランプ。
#[test]
fn arrow_transposes_by_semitone_and_octave() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 1.0, 1.0), mk_note(120, 2.0, 1.0)]]);
    show_clips(&mut app, &keys);
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: false, steps: 1 });
    assert_eq!(notes_of(&app, 0)[0].pitch, 61, "半音上");
    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: true, steps: -1 });
    assert_eq!(notes_of(&app, 0)[0].pitch, 49, "1 オクターブ下");

    // 2 音選択で上端に当てる: pitch 120 が 127 に当たるので delta 全体が縮む。
    app.selection.selected_notes =
        vec![AppData::pack_note_id(0, 0), AppData::pack_note_id(0, 1)];
    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: true, steps: 1 });
    let pitches: Vec<u8> = notes_of(&app, 0).iter().map(|n| n.pitch).collect();
    assert_eq!(pitches, vec![56, 127], "上端に当たった分だけ delta を縮める (相対 7 半音を維持)");
}

/// スケール (Fold 表示) 中の ↑/↓ は **必ず in-scale**。半音で動かすと画面の 1 行と食い違う。
#[test]
fn arrow_walks_the_scale_when_folded() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 1.0, 1.0)]]); // C4
    show_clips(&mut app, &keys);
    app.edit_song(|song| {
        song.scale_changes.push(ScaleChange { beat: 0.0, root: 0, scale: Scale::Major });
    });
    app.ui_prefs.piano_roll_fold = true;
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: false, steps: 1 });
    assert_eq!(notes_of(&app, 0)[0].pitch, 62, "C → D (C# は飛ばす)");
    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: false, steps: 2 });
    assert_eq!(notes_of(&app, 0)[0].pitch, 65, "D → E → F");
    // C メジャーは 7 音 = 1 オクターブぶんの degree。
    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: true, steps: 1 });
    assert_eq!(notes_of(&app, 0)[0].pitch, 77, "F → 1 オクターブ上の F");
}

/// Ctrl+←/→ は長さの伸縮。下限も集合クランプ (相対的な長さの比を保つ)。
#[test]
fn ctrl_arrow_resizes_notes_as_a_set() {
    let mut app = build_app();
    let keys =
        setup_tracks(&mut app, &[vec![mk_note(60, 0.0, 1.0), mk_note(64, 4.0, 0.5)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes =
        vec![AppData::pack_note_id(0, 0), AppData::pack_note_id(0, 1)];

    app.handle_event(AppEvent::NudgeSelectedNoteLength { step: NudgeStep::Grid, steps: 1 });
    let lens: Vec<f64> = notes_of(&app, 0).iter().map(|n| n.duration_beats).collect();
    for (got, want) in lens.iter().zip([1.25, 0.75]) {
        assert!((got - want).abs() < 1e-9, "両方 +0.25 拍: got {lens:?}");
    }

    // 短い方 (0.75) が下限 0.0625 に当たるまでしか縮まない。
    app.handle_event(AppEvent::NudgeSelectedNoteLength { step: NudgeStep::Bar, steps: -1 });
    let lens: Vec<f64> = notes_of(&app, 0).iter().map(|n| n.duration_beats).collect();
    for (got, want) in lens.iter().zip([0.5625, 0.0625]) {
        assert!((got - want).abs() < 1e-9, "下限で delta 全体が縮む: got {lens:?}");
    }
}

/// **クリップの端ではクランプしない** (設計判断の固定)。
///
/// daw_01 の clip は共有 content への「窓」なので、窓の外へ出たノートは鳴らない / 描かれない
/// だけでデータは無傷 — 窓を広げれば戻ってくる。他 DAW も矢印移動でクリップを伸ばさない。
/// 唯一の下限は content-local 拍 0 (model が保持できる最小位置、ドラッグと同じ)。
#[test]
fn nudge_may_push_notes_outside_the_clip_window() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 31.5, 1.0)]]); // clip 長 32 拍の末尾
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Bar, steps: 2 });
    assert!(
        (notes_of(&app, 0)[0].start_beat - 39.5).abs() < 1e-9,
        "窓 (32 拍) の外へも出られる: got {}",
        notes_of(&app, 0)[0].start_beat
    );
    // クリップ自体は伸びない (窓は不変)。
    assert!(
        (app.song_doc.song().tracks[0].clips[0].length_beats - 32.0).abs() < 1e-9,
        "クリップは伸びない"
    );
    // 戻せば窓の中へ戻る (データは無傷)。
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Bar, steps: -2 });
    assert!((notes_of(&app, 0)[0].start_beat - 31.5).abs() < 1e-9, "戻すと元位置");
}

/// ロック中トラックのノートは delta 決定からも書き込みからも外れる。
/// (delta 決定に含めると、ロック音が端に居るだけで動かせるはずの音まで止まる。)
#[test]
fn locked_notes_do_not_hold_back_the_rest_of_the_selection() {
    let mut app = build_app();
    // track0 = ロック対象 (拍 0 = 左端)、track1 = 動かしたい音 (拍 4)。
    let keys =
        setup_tracks(&mut app, &[vec![mk_note(60, 0.0, 1.0)], vec![mk_note(64, 4.0, 1.0)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.handle_event(AppEvent::TogglePianoRollTrackLock(1));
    app.selection.selected_notes =
        vec![AppData::pack_note_id(0, 0), AppData::pack_note_id(1, 0)];

    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Bar, steps: -1 });
    assert!(
        (notes_of(&app, 0)[0].start_beat - 0.0).abs() < 1e-9,
        "ロック中の音は動かない"
    );
    assert!(
        (notes_of(&app, 1)[0].start_beat - 0.0).abs() < 1e-9,
        "ロック外の音は 4 拍ぶん動く (ロック音が左端に居ても止まらない)"
    );
}

/// 連続したカーソルキー操作は **1 undo step** に畳まれる
/// (押しっぱなしで 100 step 積まれるのは誤り)。
#[test]
fn consecutive_nudges_collapse_into_one_undo_step() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 4.0, 1.0)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    let before = app.song_doc.history_current();
    for _ in 0..8 {
        app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: 1 });
    }
    assert!((notes_of(&app, 0)[0].start_beat - 6.0).abs() < 1e-9, "8 回で +2 拍");
    assert_eq!(
        app.song_doc.history_current() - before,
        1,
        "連続 nudge は 1 undo step: labels={:?}",
        app.song_doc.history_labels()
    );
    app.song_doc.undo();
    assert!((notes_of(&app, 0)[0].start_beat - 4.0).abs() < 1e-9, "1 回の undo で元位置へ戻る");
}

/// 端で止まっているときは **編集も undo step も発生させない**
/// (押しっぱなしのまま無限に undo が積まれるのを防ぐ)。
#[test]
fn nudge_at_the_edge_does_not_push_undo_steps() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 0.0, 1.0)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app);
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    let before = app.song_doc.history_current();
    for _ in 0..5 {
        app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: -1 });
    }
    assert_eq!(app.song_doc.history_current(), before, "拍 0 で止まったまま undo は積まれない");
}

/// 選択が空なら何も起きない (ユーザー決定: 再生位置移動やスクロールには割り当てない)。
#[test]
fn nudge_without_selection_is_a_no_op() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 4.0, 1.0)]]);
    show_clips(&mut app, &keys);
    let before = app.song_doc.history_current();
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: 1 });
    app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: false, steps: 1 });
    app.handle_event(AppEvent::NudgeSelectedNoteLength { step: NudgeStep::Grid, steps: 1 });
    assert!((notes_of(&app, 0)[0].start_beat - 4.0).abs() < 1e-9);
    assert_eq!(app.song_doc.history_current(), before);
}

/// Alt (Fine) はスナップ無効の微移動。スナップ OFF の無修飾 ←/→ も同じ量になる。
#[test]
fn fine_step_is_one_sixteenth_of_the_grid() {
    let mut app = build_app();
    let keys = setup_tracks(&mut app, &[vec![mk_note(60, 4.0, 1.0)]]);
    show_clips(&mut app, &keys);
    enable_grid(&mut app); // 1/16 = 0.25 拍 → fine = 0.015625 拍
    app.selection.selected_notes = vec![AppData::pack_note_id(0, 0)];

    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Fine, steps: 1 });
    assert!(
        (notes_of(&app, 0)[0].start_beat - 4.015_625).abs() < 1e-9,
        "grid 1/16 の 1/16 ぶん動く: got {}",
        notes_of(&app, 0)[0].start_beat
    );

    // スナップを切ると無修飾 ←/→ も同じ微移動になる (「グリッド」 が定義できないため)。
    app.ui_prefs.pianoroll_snap_enabled = false;
    app.handle_event(AppEvent::NudgeSelectedNoteTime { step: NudgeStep::Grid, steps: 1 });
    assert!(
        (notes_of(&app, 0)[0].start_beat - 4.031_25).abs() < 1e-9,
        "スナップ OFF の Grid = Fine: got {}",
        notes_of(&app, 0)[0].start_beat
    );
}
