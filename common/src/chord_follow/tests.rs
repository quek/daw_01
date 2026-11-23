//! `chord_follow` のテスト。期待値は仕様の例 (r.md #157 / #159、`docs/plan_chord_track.md`) の固定値。

use super::*;
use crate::audio_render::TempoMap;
use crate::chord_detect::DetectOptions;
use crate::model::{AudioEvent, ChordEvent, Clip, ClipContent, MidiContent, Note, Track};
use crate::transpose::TrackTranspose;

fn note(id: u32, start: f64, len: f64, pitch: u8) -> Note {
    Note { id, start_beat: start, duration_beats: len, pitch, velocity: 100, lyric: None, muted: false }
}

fn chord(text: &str) -> Chord {
    crate::chord_name::parse_chord(text).expect("chord")
}

fn boxed(start: f64, len: f64, text: &str) -> ChordEvent {
    ChordEvent::new(start, len, chord(text))
}

/// 曲を作る: トラック `id` ごとに、拍 0 から始まるクリップ 1 つ (中身は `notes`) とモード `mode`。
fn song_with(tracks: &[(u32, ChordFollow, Vec<Note>)]) -> Song {
    let mut song = Song::default();
    for (id, mode, notes) in tracks {
        let content_id = song.alloc_content_id();
        let next_note_id = notes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        song.clip_contents.insert(content_id, ClipContent::Midi(MidiContent { notes: notes.clone(), next_note_id }));
        let clip = Clip { id: 100 + id, start_beat: 0.0, length_beats: 16.0, content_id, ..Clip::default() };
        song.tracks.push(Track { id: *id, follow_chords: *mode, clips: vec![clip], ..Track::default() });
    }
    song
}

/// トラック `track_id` のクリップ `clip` のノートが鳴らす鍵盤 (書いた順、移調 0)。
fn heard(song: &Song, track_id: u32, clip: &Clip) -> Vec<Option<u8>> {
    let keys = TrackTranspose::of(song, track_id);
    song.clip_notes(clip).iter().map(|n| keys.key(clip, n, Some(clip.content_to_song_beat(n.start_beat)))).collect()
}

/// トラック `track_id` の最初のクリップのノートが鳴らす鍵盤。
fn sounding(song: &Song, track_id: u32) -> Vec<Option<u8>> {
    heard(song, track_id, &song.track_by_id(track_id).expect("track").clips[0])
}

fn keys(pitches: &[u8]) -> Vec<Option<u8>> {
    pitches.iter().map(|p| Some(*p)).collect()
}

/// 箱 `start` の id (無ければ panic)。
fn box_at(song: &Song, beat: f64) -> u32 {
    song.chord_at(beat).expect("box").id
}

const MODES: [ChordFollow; 4] = [ChordFollow::Parallel, ChordFollow::Narrow, ChordFollow::Bass, ChordFollow::Scale];

const PIANO: u32 = 1;
const MELODY: u32 = 2;

/// ピアノ (C の 3 和音、拍 0-4) とメロディー (E F G A の 4 分音符) を、それぞれ `piano` / `melody` のモードで。
fn piano_and_melody(piano: ChordFollow, melody: ChordFollow) -> Song {
    let mut song = song_with(&[
        (PIANO, piano, vec![note(1, 0.0, 4.0, 60), note(2, 0.0, 4.0, 64), note(3, 0.0, 4.0, 67)]),
        (MELODY, melody, vec![note(1, 0.0, 1.0, 76), note(2, 1.0, 1.0, 77), note(3, 2.0, 1.0, 79), note(4, 3.0, 1.0, 81)]),
    ]);
    song.scale_changes = vec![ScaleChange { beat: 0.0, root: 0, scale: Scale::Major }];
    song
}

/// r.md #159 のユーザーの例: ピアノが C を弾いている位置に手で Am を置くと、ピアノは Am に合わせて動き、メロディーは
/// 動かない。その後 Am を Dm に書き換えると、メロディーは Am → Dm の差だけ動き、ピアノは Dm に合わせて動く。
#[test]
fn placing_a_chord_moves_only_the_chord_part_and_rewriting_it_moves_the_melody() {
    let melody = [76u8, 77, 79, 81];
    /// (モード, Am を置いた後のピアノ, Dm に書き換えた後のピアノ, 書き換えた後のメロディー)。
    type Case = (ChordFollow, [u8; 3], [u8; 3], [u8; 4]);
    let cases: [Case; 4] = [
        (ChordFollow::Parallel, [57, 61, 64], [62, 66, 69], [81, 82, 84, 86]),
        (ChordFollow::Narrow, [60, 64, 69], [62, 65, 69], [77, 77, 79, 81]),
        (ChordFollow::Bass, [57, 61, 64], [62, 66, 69], [81, 82, 84, 86]),
        // C メジャーのキーの中のコード同士 (Am → Dm) は寄せ先 (キーの音階) が同じで、C の和音はもう中にあるので動かない。
        (ChordFollow::Scale, [60, 64, 67], [60, 64, 67], melody),
    ];
    for (mode, piano_am, piano_dm, melody_dm) in cases {
        let mut song = piano_and_melody(mode, mode);
        song.place_chord(boxed(0.0, 4.0, "Am"));
        assert_eq!(sounding(&song, PIANO), keys(&piano_am), "{mode:?}: ピアノは Am へ");
        assert_eq!(sounding(&song, MELODY), keys(&melody), "{mode:?}: 置いただけではメロディーは動かない");
        let id = song.chords[0].id;
        assert!(song.set_chord(&[id], chord("Dm")));
        assert_eq!(sounding(&song, PIANO), keys(&piano_dm), "{mode:?}: ピアノは Dm へ");
        assert_eq!(sounding(&song, MELODY), keys(&melody_dm), "{mode:?}: メロディーは Am → Dm の差");
    }
}

/// r.md #159 の元の場面: ピアノのクリップからコードを抽出しただけでは、メロディーもピアノも動かない。抽出したコード列を
/// +2 移調すると、平行移動のピアノとメロディーが +2 動く。
#[test]
fn extracting_chords_moves_nothing_and_transposing_them_moves_everything() {
    let mut song = piano_and_melody(ChordFollow::Parallel, ChordFollow::Parallel);
    let extracted = song.chords_from_clips(&[(PIANO, 100 + PIANO)], &DetectOptions::default());
    assert!(song.place_chords(extracted));
    assert_eq!(song.chords.len(), 1);
    for mode in MODES {
        for t in &mut song.tracks {
            t.follow_chords = mode;
        }
        assert_eq!(sounding(&song, PIANO), keys(&[60, 64, 67]), "{mode:?}");
        assert_eq!(sounding(&song, MELODY), keys(&[76, 77, 79, 81]), "{mode:?}");
    }
    for t in &mut song.tracks {
        t.follow_chords = ChordFollow::Parallel;
    }
    assert!(song.transpose_chords(&[], 2));
    assert_eq!(sounding(&song, PIANO), keys(&[62, 66, 69]));
    assert_eq!(sounding(&song, MELODY), keys(&[78, 79, 81, 83]));
}

/// 和音のパートは今どおり (#157 の式): C の和音の位置に Am7 を置くと平行移動のピアノは A へ動く。抽出した C を Am7 に
/// 書き換えると、ピアノもメロディーも同じだけ (-3) 動く (和音のパートだけ取り残されてメロディーとぶつからない)。
#[test]
fn the_chord_part_and_the_melody_move_together_on_a_rewrite() {
    let mut song = piano_and_melody(ChordFollow::Parallel, ChordFollow::Parallel);
    song.place_chord(boxed(0.0, 4.0, "Am7"));
    assert_eq!(sounding(&song, PIANO), keys(&[57, 61, 64]));
    assert_eq!(sounding(&song, MELODY), keys(&[76, 77, 79, 81]));
    let mut song = piano_and_melody(ChordFollow::Parallel, ChordFollow::Parallel);
    let extracted = song.chords_from_clips(&[(PIANO, 100 + PIANO)], &DetectOptions::default());
    assert!(song.place_chords(extracted));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("Am7")));
    assert_eq!(sounding(&song, PIANO), keys(&[57, 61, 64]));
    assert_eq!(sounding(&song, MELODY), keys(&[73, 74, 76, 78]));
}

/// 単音の旋律 (経過音を含む) は、どのモードでも、どんなコードを手で置いても、キーを変えても動かない。
#[test]
fn a_melody_never_moves_when_chords_are_only_placed() {
    let melody: Vec<Note> = [72u8, 74, 76, 77, 79, 78, 76, 73].iter().zip(1u32..).map(|(&p, id)| {
        note(id, f64::from(id - 1) * 0.5, 0.5, p)
    }).collect();
    let written: Vec<Option<u8>> = melody.iter().map(|n| Some(n.pitch)).collect();
    for mode in MODES {
        let mut song = song_with(&[(MELODY, mode, melody.clone())]);
        for (start, text) in [(0.0, "Am"), (2.0, "F#m7b5"), (4.0, "Eb/G")] {
            song.place_chord(boxed(start, 2.0, text));
            assert_eq!(sounding(&song, MELODY), written, "{mode:?}: {text} を置いた");
        }
        song.scale_changes = vec![ScaleChange { beat: 0.0, root: 3, scale: Scale::NaturalMinor }];
        assert_eq!(sounding(&song, MELODY), written, "{mode:?}: キーを変えた");
    }
}

/// 2 拍の窓に集めると C になる旋律 (C E G を重ねずに) は、和音として動かない。同じ音を伸ばして重ねると和音として動く。
#[test]
fn melody_notes_in_a_window_are_not_mistaken_for_a_chord() {
    let spread = vec![note(1, 0.0, 0.5, 60), note(2, 0.5, 0.5, 64), note(3, 1.0, 0.5, 67)];
    let held = vec![note(1, 0.0, 2.0, 60), note(2, 0.5, 1.5, 64), note(3, 1.0, 1.0, 67)];
    let mut song = song_with(&[(1, ChordFollow::Parallel, spread), (2, ChordFollow::Parallel, held)]);
    song.place_chord(boxed(0.0, 4.0, "Am"));
    assert_eq!(sounding(&song, 1), keys(&[60, 64, 67]));
    assert_eq!(sounding(&song, 2), keys(&[57, 61, 64]));
}

/// 旋律の後に和音が 1 つ混ざったクリップに Am を置くと、動くのは和音と、和音に隙間なく接する音だけ (旋律全体を和音の
/// パートにしない)。
#[test]
fn one_chord_in_a_melody_does_not_make_the_whole_melody_a_chord_part() {
    let mut notes: Vec<Note> = [72u8, 74, 76, 77, 79].iter().zip(1u32..).map(|(&p, id)| note(id, f64::from(id - 1), 1.0, p)).collect();
    notes.extend([note(6, 5.0, 1.0, 60), note(7, 5.0, 1.0, 64), note(8, 5.0, 1.0, 67)]);
    for (mode, want) in [
        (ChordFollow::Parallel, [72u8, 74, 76, 77, 76, 57, 61, 64]),
        (ChordFollow::Narrow, [72, 74, 76, 77, 81, 60, 64, 69]),
    ] {
        let mut song = song_with(&[(MELODY, mode, notes.clone())]);
        song.place_chord(boxed(0.0, 8.0, "Am"));
        assert_eq!(sounding(&song, MELODY), keys(&want), "{mode:?}");
    }
}

/// クリップ `clip` の、窓に頭があって鳴るノートの鍵盤 (書いた順)。
fn audible(song: &Song, track_id: u32, clip: &Clip) -> Vec<Option<u8>> {
    let all = heard(song, track_id, clip);
    song.clip_notes(clip).iter().zip(all).filter(|(n, _)| clip.window_has_onset(n.start_beat)).map(|(_, k)| k).collect()
}

/// 自分の和音はトラックが **鳴らす** ノートだけから決める: 窓の外に隠れた和音に接する旋律の音も、隠れたベース音と
/// 終わりが揃う 2 音も、和音のパートにしない (鳴っていない音で動かない)。分割しても鳴る音は変わらない。
#[test]
fn hidden_notes_outside_the_window_do_not_make_a_chord_part() {
    let mut notes: Vec<Note> = [72u8, 74, 76, 77, 79].iter().zip(1u32..).map(|(&p, id)| note(id, f64::from(id - 1), 1.0, p)).collect();
    notes.extend([note(6, 5.0, 1.0, 60), note(7, 5.0, 1.0, 64), note(8, 5.0, 1.0, 67)]);
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, notes)]);
    song.place_chord(boxed(0.0, 8.0, "Am"));
    let whole = keys(&[72, 74, 76, 77, 76, 57, 61, 64]);
    assert_eq!(sounding(&song, MELODY), whole, "和音と、それに接する 79 が動く");
    let mut split = song.clone();
    split.split_clips_at(3.0);
    let track = split.track_by_id(MELODY).expect("track");
    let pieces: Vec<Option<u8>> = track.clips.iter().flat_map(|c| audible(&split, MELODY, c)).collect();
    assert_eq!(pieces, whole, "分割の前後で鳴る音は同じ");
    song.tracks[0].clips[0].length_beats = 5.0;
    assert_eq!(audible(&song, MELODY, &song.tracks[0].clips[0]), keys(&[72, 74, 76, 77, 79]), "隠れた和音に接しても動かない");

    // 左端を詰めて頭を隠したベース音 C2 と、窓の中の E G (2 音は和音にならない)。
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 4.0, 36), note(2, 1.0, 3.0, 64), note(3, 1.0, 3.0, 67)])]);
    song.place_chord(boxed(0.0, 8.0, "Am"));
    assert_eq!(sounding(&song, MELODY), keys(&[33, 61, 64]), "ベース音ごと C の和音として動く");
    let clip = &mut song.tracks[0].clips[0];
    (clip.start_beat, clip.content_offset_beats, clip.length_beats) = (1.0, 1.0, 3.0);
    assert_eq!(audible(&song, MELODY, &song.tracks[0].clips[0]), keys(&[64, 67]), "隠れたベース音で和音にしない");
}

/// 「箱を置き直す (新しい系統) → 書き換える」を繰り返しても、開いている間の記録は積み増さない (もう効かない系統の
/// 記録は、記録を足すときに落とす)。鳴る音は最後の書き換えの差だけ。
#[test]
fn re_placing_and_rewriting_does_not_pile_up_records() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64), note(2, 1.0, 1.0, 67)])]);
    for _ in 0..4 {
        song.place_chord(boxed(0.0, 4.0, "C"));
        assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
        assert_eq!(song.tracks[0].clips[0].chord_basis.notes.len(), 2, "ノート × 今ある系統");
    }
    assert_eq!(sounding(&song, MELODY), keys(&[66, 69]));
}

/// 書き換えは窓の外へ隠したノートにも記録を書く: 後で窓を広げて出したとき、そこだけ動かないまま残らない。
#[test]
fn rewrites_remember_notes_hidden_outside_the_window() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64), note(2, 4.0, 1.0, 67)])]);
    let clip = &mut song.tracks[0].clips[0];
    (clip.start_beat, clip.content_offset_beats, clip.length_beats) = (4.0, 4.0, 4.0);
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "C"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")), "隠れたノートの下の箱だけを書き換える");
    let clip = &mut song.tracks[0].clips[0];
    (clip.start_beat, clip.content_offset_beats, clip.length_beats) = (0.0, 0.0, 8.0);
    assert_eq!(sounding(&song, MELODY), keys(&[66, 67]), "出したノートも C → D");
}

/// ベース: 転回形の和音 (C/E) のパートからコードを抽出した直後 (同じコードの箱の下) は動かず、根音がベースの C へ
/// 書き換えると、ベース音 E → C の差で動く (Studio One: 元のコードのベース音 → 今のコードのベース音)。
#[test]
fn an_inverted_chord_part_stays_under_its_own_extracted_chord_in_bass_mode() {
    let mut song = song_with(&[(PIANO, ChordFollow::Bass, vec![note(1, 0.0, 4.0, 52), note(2, 0.0, 4.0, 55), note(3, 0.0, 4.0, 60)])]);
    let own = own_chords_in(&song, OwnScope::Arrangement { track_id: PIANO }, None);
    assert_eq!(own.len(), 1);
    assert_eq!((own[0].chord.root, own[0].chord.bass_pc()), (0, 4), "C/E と読む");
    song.place_chord(ChordEvent::new(0.0, 4.0, own[0].chord));
    assert_eq!(sounding(&song, PIANO), keys(&[52, 55, 60]), "抽出した直後は動かない");
    song.place_chord(boxed(0.0, 4.0, "C"));
    assert_eq!(sounding(&song, PIANO), keys(&[48, 51, 56]), "E → C (-4)");
}

/// ベース音のあとに和音を刻む伴奏 (ブンチャッ) は、ベース音も和音のパートとして一緒に動く。食って入る和音
/// (小節の頭の半拍前から弾く) は次の箱のコードに従う。
#[test]
fn accompaniment_figures_move_as_one_chord_part() {
    let oom_pah = vec![note(1, 0.0, 1.0, 48), note(2, 1.0, 1.0, 60), note(3, 1.0, 1.0, 64), note(4, 1.0, 1.0, 67)];
    let anticipated = vec![
        note(1, 0.0, 3.5, 60), note(2, 0.0, 3.5, 64), note(3, 0.0, 3.5, 67),
        note(4, 3.5, 4.5, 65), note(5, 3.5, 4.5, 69), note(6, 3.5, 4.5, 72),
    ];
    let mut song = song_with(&[(1, ChordFollow::Parallel, oom_pah), (2, ChordFollow::Parallel, anticipated)]);
    song.place_chord(boxed(0.0, 4.0, "Am"));
    assert_eq!(sounding(&song, 1), keys(&[45, 57, 61, 64]));
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "F"));
    assert_eq!(sounding(&song, 2), keys(&[60, 64, 67, 65, 69, 72]), "食って入る F は F の箱のまま");
}

/// 書き換えた後に足したノートは置いた時点のコード (書き換えた後のコード) が基準で、次の書き換えの差だけ動く。
/// 書き換えを重ねても記録は最初のコードのまま (C → Dm → Em なら C → Em)、元のコードへ書き戻すと動かない。
#[test]
fn material_added_after_a_rewrite_only_follows_later_rewrites() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64)])]);
    song.place_chord(boxed(0.0, 4.0, "C"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("Dm")));
    let content = song.tracks[0].clips[0].content_id;
    if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&content) {
        m.notes.push(note(2, 1.0, 1.0, 69));
    }
    assert_eq!(sounding(&song, MELODY), keys(&[66, 69]), "足したノートは書いた音");
    assert!(song.set_chord(&[id], chord("Em")));
    assert_eq!(sounding(&song, MELODY), keys(&[68, 71]), "C → Em (+4) と Dm → Em (+2)");
    assert!(song.set_chord(&[id], chord("C")));
    assert_eq!(sounding(&song, MELODY), keys(&[64, 67]), "C へ戻すと最初の音、足した音は Dm → C");
}

/// 追従しないトラックと、曲全体のスイッチが切れている間の書き換えも記録する (記録は素材の事実): 後で ON にすると、
/// ずっと ON だったときと同じ音で鳴る。
#[test]
fn rewrites_while_following_is_off_are_remembered() {
    let mut song = song_with(&[(MELODY, ChordFollow::Off, vec![note(1, 0.0, 1.0, 64)]), (3, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64)])]);
    song.place_chord(boxed(0.0, 4.0, "C"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("Dm")));
    song.chord_follow_enabled = false;
    assert!(song.set_chord(&[id], chord("G")));
    assert_eq!((sounding(&song, MELODY), sounding(&song, 3)), (keys(&[64]), keys(&[64])), "Off の間は書いた音");
    song.chord_follow_enabled = true;
    song.tracks[0].follow_chords = ChordFollow::Parallel;
    assert_eq!((sounding(&song, MELODY), sounding(&song, 3)), (keys(&[59]), keys(&[59])), "C → G");
}

/// 書き換えた箱をずらしても、重なっていた部分のメロディーは今の箱のコードへ合わせて鳴り、新しく覆ったメロディーは
/// 動かない。内側に箱を置いて割ると、置いた箱の下は置いただけなので書いた音、割れた右側は元の箱の続き。
#[test]
fn moving_or_splitting_a_rewritten_box_keeps_the_melody_on_the_current_chord() {
    let melody: Vec<Note> = (0..8u32).map(|i| note(i + 1, f64::from(i), 1.0, 64)).collect();
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, melody)]);
    song.place_chord(boxed(0.0, 4.0, "C"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("D")));
    assert_eq!(sounding(&song, MELODY), keys(&[66, 66, 66, 66, 64, 64, 64, 64]));
    assert!(song.move_chords(&[id], 1.0));
    assert_eq!(sounding(&song, MELODY), keys(&[64, 66, 66, 66, 64, 64, 64, 64]), "拍 4 の新しく覆った音は動かない");
    song.place_chord(boxed(2.0, 1.0, "E"));
    assert_eq!(sounding(&song, MELODY), keys(&[64, 66, 64, 66, 64, 64, 64, 64]), "内側の E は置いただけ");
}

/// 記録は箱の系統と組: 書き換えで動いたメロディーのクリップを別の箱の下へ動かす / その上に箱を置く / 箱を貼る・複製
/// すると書いた音で鳴る (置いただけでは動かない)。同じ中身のクリップは、書き換えを経験したかどうかによらず同じ音で鳴る。
/// 元の箱の下へ戻すと元どおり動く。
#[test]
fn memories_belong_to_the_box_they_were_made_under() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64), note(2, 1.0, 1.0, 67)])]);
    let twin = Clip { id: 200, start_beat: 8.0, length_beats: 4.0, ..song.tracks[0].clips[0].clone() };
    song.tracks[0].clips[0].length_beats = 4.0;
    song.tracks[0].clips.push(twin);
    song.place_chord(boxed(0.0, 4.0, "C"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("Dm")));
    assert_eq!(sounding(&song, MELODY), keys(&[66, 69]));
    // 別の箱 (G を置いた / 貼った / 複製した) の下へ動かすと書いた音。同じ中身の twin も同じ音。
    song.place_chord(boxed(16.0, 4.0, "G"));
    let dup = song.duplicate_chords(&[box_at(&song, 16.0)], 8.0);
    assert_eq!(dup.len(), 1);
    for at in [16.0, 24.0] {
        for i in 0..2 {
            song.tracks[0].clips[i].start_beat = at;
            assert_eq!(heard(&song, MELODY, &song.tracks[0].clips[i]), keys(&[64, 67]), "{at}: clip {i}");
        }
    }
    // 元の箱の下へ戻すと元どおり。
    song.tracks[0].clips[0].start_beat = 0.0;
    assert_eq!(sounding(&song, MELODY), keys(&[66, 69]));
    // 書き換えた箱の上にコードを置き直す (同じ範囲に新しい箱) と、置いただけなので書いた音。
    song.place_chord(boxed(0.0, 4.0, "A"));
    assert_eq!(sounding(&song, MELODY), keys(&[64, 67]));
}

/// 時間ごと複製 (Duplicate Time) した区間は元と同じ音で鳴り、その後に貼った側を書き換えても元の区間は変わらない。
#[test]
fn duplicated_time_sounds_the_same_and_is_independent_afterwards() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64), note(2, 1.0, 1.0, 67)])]);
    song.tracks[0].clips[0].length_beats = 4.0;
    song.place_chord(boxed(0.0, 4.0, "C"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
    assert!(song.duplicate_time_range(0.0, 4.0).is_some());
    let track = song.track_by_id(MELODY).expect("track");
    let copy = track.clips.iter().find(|c| c.start_beat == 4.0).expect("貼ったクリップ").clone();
    assert_eq!(heard(&song, MELODY, &copy), keys(&[66, 69]), "貼った区間も C → D");
    assert!(song.set_chord(&[box_at(&song, 4.0)], chord("E")));
    let track = song.track_by_id(MELODY).expect("track");
    let copy = track.clips.iter().find(|c| c.start_beat == 4.0).expect("貼ったクリップ").clone();
    assert_eq!(heard(&song, MELODY, &copy), keys(&[68, 71]));
    assert_eq!(sounding(&song, MELODY), keys(&[66, 69]), "元の区間は変わらない");
}

/// 書き換えで動いたノートを跨いでクリップを割っても (共有している content は fork される)、後ろの片は元のノートと
/// 同じく動いたまま鳴る。共有していた別の位置のクリップは変わらない。
#[test]
fn splitting_a_clip_keeps_the_rewritten_sound_of_both_pieces() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 4.0, 64), note(2, 0.0, 4.0, 76)])]);
    let linked = Clip { id: 200, start_beat: 8.0, ..song.tracks[0].clips[0].clone() };
    song.tracks[0].clips[0].length_beats = 4.0;
    song.tracks[0].clips.push(Clip { length_beats: 4.0, ..linked });
    song.place_chord(boxed(0.0, 4.0, "C"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("D")));
    song.split_clips_at(2.0);
    let track = song.track_by_id(MELODY).expect("track");
    let back = track.clips.iter().find(|c| c.start_beat == 2.0).expect("後ろの片");
    assert_ne!(back.content_id, track.clips.iter().find(|c| c.id == 200).expect("linked").content_id, "fork される");
    assert_eq!(audible(&song, MELODY, back), keys(&[66, 78]), "後ろの片も C → D");
    assert_eq!(audible(&song, MELODY, &track.clips[0]), keys(&[66, 78]));
}

/// 時間操作で割れた同じ系統の箱の境界 (音源フレームの途中に落ちる: 128 BPM / 44.1 kHz) では、どちらの片を先に
/// 書き換えても、境界ちょうどから後ろの片の記録を引き、直前は前の片の記録を引く (1 フレームぶん隣の記録を引かない)。
#[test]
fn audio_records_meet_exactly_at_a_split_box_boundary() {
    use crate::model::{AudioContent, AudioSource, AudioSourcePath};
    for back_first in [false, true] {
        let mut song = Song { bpm: 128.0, ..Song::default() };
        song.media.audio_sources.insert(1, AudioSource {
            path: AudioSourcePath::Absolute("1.wav".into()),
            sample_rate: 44_100,
            channels: 1,
            frames: 165_375,
            original_bpm: None,
            root_key: None,
        });
        let event = AudioEvent {
            id: 1,
            source_id: 1,
            event_length_beats: 6.0,
            source_end_frames: 124_031,
            stretch_mode: crate::model::StretchMode::Raw,
            ..AudioEvent::default()
        };
        song.place_chord(boxed(0.0, 8.0, "C"));
        assert!(song.delete_time_range(2.0, 4.0).is_some());
        assert_eq!(song.chords.len(), 2);
        assert_eq!(song.chords[0].lineage, song.chords[1].lineage, "割れた片は系統を継ぐ");
        // 素材が無いうちに後ろの片を E へ (記録は付かない)。
        assert!(song.set_chord(&[box_at(&song, 3.0)], chord("E")));
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(content_id, ClipContent::Audio(AudioContent { events: vec![event.clone()], next_event_id: 2 }));
        let clip = Clip { id: 1, start_beat: 0.0, length_beats: 6.0, content_id, ..Clip::default() };
        song.tracks.push(Track { id: 1, follow_chords: ChordFollow::Parallel, clips: vec![clip], ..Track::default() });
        let (front, back) = (box_at(&song, 1.0), box_at(&song, 3.0));
        let order = if back_first { [(back, "G"), (front, "F")] } else { [(front, "F"), (back, "G")] };
        for (id, to) in order {
            assert!(song.set_chord(&[id], chord(to)));
        }
        let shifts = |beats: &[f64]| {
            let basis = song.audio_event_basis(&TempoMap::from_song(&song), &song.tracks[0].clips[0], &event);
            let f = TrackChordFollow::of(&song, 1);
            beats.iter().map(|&at| f.audio_shift(at, None, &basis)).collect::<Vec<_>>()
        };
        assert_eq!(shifts(&[1.0, 2.0 - 1e-6, 2.0, 2.5]), vec![5, 5, 3, 3], "back_first={back_first}: C → F / E → G");
    }
}

/// 120 BPM / 48 kHz / 8 拍の音源 `source_id` を Raw で鳴らす event (クリップの頭から)。1 拍 = 24 000 frame。
fn audio_event(song: &mut Song, source_id: u32) -> AudioEvent {
    use crate::model::{AudioSource, AudioSourcePath};
    song.media.audio_sources.insert(source_id, AudioSource {
        path: AudioSourcePath::Absolute(format!("{source_id}.wav").into()),
        sample_rate: 48_000,
        channels: 1,
        frames: 192_000,
        original_bpm: None,
        root_key: None,
    });
    AudioEvent {
        id: 1,
        source_id,
        event_length_beats: 8.0,
        source_end_frames: 192_000,
        stretch_mode: crate::model::StretchMode::Raw,
        ..AudioEvent::default()
    }
}

/// クリップ `clip` (の event `event`) を鳴らすトラック `track_id` の、曲の拍 `beats` での移動量 (engine と同じ口)。
fn audio_shifts(song: &Song, track_id: u32, clip: &Clip, event: &AudioEvent, beats: &[f64]) -> Vec<i32> {
    let basis = song.audio_event_basis(&TempoMap::from_song(song), clip, event);
    let f = TrackChordFollow::of(song, track_id);
    let spans = song.audio_chords.get(&event.source_id).map_or(&[][..], Vec::as_slice);
    beats
        .iter()
        .map(|&at| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let frame = ((at - clip.start_beat).clamp(0.0, 7.99) * 24_000.0) as u64;
            f.audio_shift(at, crate::chord_detect::audio_own_chord_at(spans, frame), &basis)
        })
        .collect()
}

/// オーディオ: 和音を弾いていない音源にコードを置いただけでは動かず、書き換えるとその差だけ動く (記録は take の
/// 音源フレームと箱の系統)。書き換えの後に複製したクリップを別の箱の下へ置いても動かない。書き換えた箱を伸ばして
/// 新しく覆った部分も動かない。和音を弾いている区間 (`chordal`) があれば、それが基準になる。
#[test]
fn audio_follows_only_rewrites_unless_it_plays_a_chord() {
    use crate::model::{AudioChordSpan, AudioContent};
    let mut song = Song::default();
    let event = audio_event(&mut song, 1);
    let content_id = song.alloc_content_id();
    song.clip_contents.insert(content_id, ClipContent::Audio(AudioContent { events: vec![event.clone()], next_event_id: 2 }));
    let clip = Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id, ..Clip::default() };
    song.tracks.push(Track { id: 1, follow_chords: ChordFollow::Parallel, clips: vec![clip], ..Track::default() });
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "F"));
    let shifts = |song: &Song, i: usize, beats: &[f64]| audio_shifts(song, 1, &song.tracks[0].clips[i], &event, beats);
    assert_eq!(shifts(&song, 0, &[1.0, 5.0]), vec![0, 0], "置いただけ");
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
    assert_eq!(shifts(&song, 0, &[1.0, 5.0]), vec![2, 0], "書き換えた箱の下だけ");
    // 書き換えた D を 8 拍へ伸ばす: 新しく覆った後半は記録が無いので動かない。
    assert!(song.resize_chord(box_at(&song, 0.0), 0.0, 8.0));
    assert_eq!(shifts(&song, 0, &[1.0, 5.0]), vec![2, 0], "伸ばして覆った部分は動かない");
    // 書き換えの後に複製したクリップ (記録ごと運ぶ) を、置いた E の箱の下へ置いても動かない。
    let mut later = song.tracks[0].clips[0].clone();
    later.id = 2;
    later.start_beat = 8.0;
    song.tracks[0].clips.push(later);
    song.place_chord(boxed(8.0, 8.0, "E"));
    assert_eq!(shifts(&song, 1, &[9.0, 13.0]), vec![0, 0], "別の箱の下へ複製したクリップは動かない");
    // 素材が Am を弾いている (前半 4 拍): 自分の和音が基準 (Am → D = +5)。後半は記録が無く書いた音。
    let span = AudioChordSpan { start_frame: 0, end_frame: 96_000, chord: chord("Am"), chordal: true };
    song.audio_chords.insert(1, vec![span]);
    assert_eq!(shifts(&song, 0, &[1.0, 5.0]), vec![5, 0]);
}

/// クロスフェードの張り出し (クリップの窓の外で鳴る尻) は、窓の端の箱と記録で鳴る (張り出しの間だけ音程が戻らない)。
#[test]
fn audio_overhang_keeps_the_shift_at_the_window_edge() {
    use crate::model::AudioContent;
    let mut song = Song::default();
    let event = audio_event(&mut song, 1);
    let content_id = song.alloc_content_id();
    song.clip_contents.insert(content_id, ClipContent::Audio(AudioContent { events: vec![event.clone()], next_event_id: 2 }));
    let clip = Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id, ..Clip::default() };
    song.tracks.push(Track { id: 1, follow_chords: ChordFollow::Parallel, clips: vec![clip], ..Track::default() });
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "F"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
    assert_eq!(audio_shifts(&song, 1, &song.tracks[0].clips[0], &event, &[3.9, 4.1]), vec![2, 2]);
}

/// 焼き込み (バウンス) は、素材が動いていた元のコードを焼いた音源へ写す: 書き換えで動いたメロディーも、帯のコードへ
/// 合わせて動いていた和音のパートも、焼いた音が同じ量だけ動く。動いていなかった素材は写さない。
#[test]
fn baking_copies_how_the_material_moved() {
    use crate::model::ClipKey;
    let mut song = piano_and_melody(ChordFollow::Parallel, ChordFollow::Parallel);
    song.place_chord(boxed(0.0, 4.0, "C"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
    song.place_chord(boxed(4.0, 4.0, "Am"));
    if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&song.tracks[0].clips[0].content_id) {
        m.notes.extend([note(4, 4.0, 4.0, 60), note(5, 4.0, 4.0, 64), note(6, 4.0, 4.0, 67)]);
    }
    let event = audio_event(&mut song, 9);
    let baked = Clip { id: 50, start_beat: 0.0, length_beats: 8.0, ..Clip::default() };
    let shifts = |track: u32| -> Vec<i32> {
        let key = ClipKey { track_id: track, clip_id: 100 + track };
        let clip = Clip { chord_basis: song.baked_chord_basis(&[key], &baked, &event), ..baked.clone() };
        audio_shifts(&song, track, &clip, &event, &[1.0, 5.0])
    };
    // メロディー: 前半は書き換え C → D (+2)、後半は置いただけ (0)。
    assert_eq!(shifts(MELODY), vec![2, 0]);
    // ピアノ: 前半は自分の和音 C → D (+2)、後半の C の和音は Am へ合わせて動いていた (-3)。
    assert_eq!(shifts(PIANO), vec![2, -3]);
}

/// Bounce In Place の記録は、同じ窓を見る linked clip ごとにその位置で決める: 書き換えた箱の下のクリップは C → D で
/// +2、置いただけの箱の下のクリップは 0 (どちらも焼いた音源へ張り替わる)。
#[test]
fn bouncing_in_place_bakes_each_linked_clip_at_its_own_position() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 4.0, 64)])]);
    song.tracks[0].clips[0].length_beats = 4.0;
    let linked = Clip { id: 200, start_beat: 8.0, ..song.tracks[0].clips[0].clone() };
    song.tracks[0].clips.push(linked);
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(8.0, 4.0, "F"));
    assert!(song.set_chord(&[box_at(&song, 0.0)], chord("D")));
    let event = audio_event(&mut song, 9);
    let content_id = song.tracks[0].clips[0].content_id;
    let bases = song.baked_window_bases(content_id, (0.0, 4.0), &event);
    assert_eq!(bases.len(), 2);
    let heard = |clip_id: u32, beat: f64| {
        let (_, _, basis) = bases.iter().find(|(_, id, _)| *id == clip_id).expect("clip");
        let clip = Clip { chord_basis: basis.clone(), ..song.tracks[0].clip_by_id(clip_id).expect("clip").clone() };
        audio_shifts(&song, MELODY, &clip, &event, &[beat])[0]
    };
    assert_eq!((heard(100 + MELODY, 1.0), heard(200, 9.0)), (2, 0));
}

/// 呼び名だけを変えた書き換え (C6 → Am7、構成音が同じ) ではメロディーは動かない。どのコードでも、置いた時点の
/// コードと同じコードの下では書いた音のまま。
#[test]
fn the_placed_chord_itself_never_moves_anything() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64), note(2, 1.0, 1.0, 66)])]);
    song.place_chord(boxed(0.0, 4.0, "C6"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("Am7")));
    assert_eq!(sounding(&song, MELODY), keys(&[64, 66]));
    let key = ScaleChange { beat: 0.0, root: 2, scale: Scale::Major };
    for &kind in ChordKind::ALL {
        for root in [0u8, 4, 11] {
            for bass in [None, Some((root + 7) % 12)] {
                let mut c = Chord::new(root, kind);
                c.bass = bass;
                c.tensions = crate::model::Tensions::NINE;
                let c = c.sanitized();
                for mode in MODES {
                    let placed = Basis { own: None, placed: Some(&c) };
                    for p in 0..=127u8 {
                        for key in [None, Some(&key)] {
                            assert_eq!(follow_pitch(mode, p, &c, placed, key), Some(p), "{mode:?} {c:?} {p}");
                        }
                    }
                    assert_eq!(follow_shift(mode, &c, placed, None), 0);
                    // 和音のパートも、自分の和音と同じコードの箱の下では全体を動かさない (転回形でも。ベースは
                    // 元のベース音 → 今のベース音、Studio One の Bass)。
                    assert_eq!(follow_shift(mode, &c, Basis::own(&c), None), 0, "{mode:?} {c:?}");
                }
            }
        }
    }
}

/// 曲全体のスイッチを切ると、トラックのモードは残ったまま全部書いた音に戻る。
#[test]
fn the_song_switch_turns_every_track_off() {
    let mut song = piano_and_melody(ChordFollow::Narrow, ChordFollow::Narrow);
    song.place_chord(boxed(0.0, 4.0, "Am"));
    assert_eq!(sounding(&song, PIANO), keys(&[60, 64, 69]));
    song.chord_follow_enabled = false;
    assert_eq!(sounding(&song, PIANO), keys(&[60, 64, 67]));
    assert_eq!(song.tracks[0].follow_chords, ChordFollow::Narrow);
}

/// スケール: ダイアトニックなコードはキーの音階へ、キーに無いコードはコードの種類のスケールへ寄せる。
#[test]
fn scale_mode_prefers_the_key_for_diatonic_chords() {
    let key = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
    let c = chord("C");
    // C メジャーキーの Am (C の和音のパートから): C# は C か D へ (同距離なら上の D)。
    assert_eq!(follow_pitch(ChordFollow::Scale, 61, &chord("Am"), Basis::own(&c), Some(&key)), Some(62));
    // キーに無い E メジャー (G# を持つ): E メジャースケールへ寄せるので F は F# へ。
    assert_eq!(follow_pitch(ChordFollow::Scale, 65, &chord("E"), Basis::own(&c), Some(&key)), Some(66));
}

/// 箱の境界の直前 (拍の丸め) に始まるノートは次の箱を引く。同じ拍に Key の変更があれば Key も変更後を引く:
/// D は C メジャーキーに無い (D メジャースケールへ寄せ、C# はそのまま) が、G メジャーキーにはある (C# は D へ)。
#[test]
fn a_note_just_before_a_boundary_takes_the_key_change_on_that_boundary() {
    let mut song = Song::default();
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "D"));
    song.scale_changes = vec![
        ScaleChange { beat: 0.0, root: 0, scale: Scale::Major },
        ScaleChange { beat: 4.0, root: 7, scale: Scale::Major },
    ];
    song.tracks.push(Track { id: 1, follow_chords: ChordFollow::Scale, ..Track::default() });
    let f = TrackChordFollow::of(&song, 1);
    // 自分の和音 (Bb のパートの一音) から寄せる。
    let own = [SourceChord { start_beat: 0.0, len_beats: 8.0, chord: chord("Bb") }];
    let src = |start: f64| NoteSource { own: &own, basis: &[], note_id: 1, start_beat: start, len_beats: 1.0 };
    assert_eq!(f.key(61, 4.0, &src(4.0)), Some(62));
    assert_eq!(f.key(61, 4.0 - 1e-9, &src(4.0 - 1e-9)), Some(62));
}

/// 範囲外へ出たら鳴らさない (端に丸めない)。
#[test]
fn out_of_range_is_silent() {
    assert_eq!(follow_pitch(ChordFollow::Parallel, 2, &chord("G"), Basis::own(&chord("C")), None), None);
    assert_eq!(follow_pitch(ChordFollow::Parallel, 2, &chord("G"), Basis { own: None, placed: Some(&chord("C")) }, None), None);
}
