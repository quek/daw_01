//! `chord_follow` のテスト。期待値は仕様の例 (r.md #157 / #159、`docs/plan_chord_track.md`) の固定値。

use super::*;
use crate::chord_detect::DetectOptions;
use crate::model::{ChordEvent, Clip, ClipChordBasis, ClipContent, MidiContent, Note, Track};
use crate::transpose::TrackTranspose;

fn note(id: u32, start: f64, len: f64, pitch: u8) -> Note {
    Note { id, start_beat: start, duration_beats: len, pitch, velocity: 100, lyric: None, muted: false }
}

fn chord(text: &str) -> Chord {
    crate::chord_name::parse_chord(text).expect("chord")
}

fn boxed(start: f64, len: f64, text: &str) -> ChordEvent {
    ChordEvent { id: 0, start_beat: start, len_beats: len, chord: chord(text), color: None }
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

/// トラック `track_id` のクリップのノートが鳴らす鍵盤 (書いた順、移調 0)。
fn sounding(song: &Song, track_id: u32) -> Vec<Option<u8>> {
    let keys = TrackTranspose::of(song, track_id);
    let clip = &song.track_by_id(track_id).expect("track").clips[0];
    song.clip_notes(clip).iter().map(|n| keys.key(clip, n, Some(clip.content_to_song_beat(n.start_beat)))).collect()
}

fn keys(pitches: &[u8]) -> Vec<Option<u8>> {
    pitches.iter().map(|p| Some(*p)).collect()
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
/// 追従しないトラックには記録しない (後で追従を入れても、その時点のコードが基準)。
#[test]
fn material_added_after_a_rewrite_only_follows_later_rewrites() {
    let mut song = song_with(&[(MELODY, ChordFollow::Parallel, vec![note(1, 0.0, 1.0, 64)]), (3, ChordFollow::Off, vec![note(1, 0.0, 1.0, 64)])]);
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
    song.tracks[1].follow_chords = ChordFollow::Parallel;
    assert_eq!(sounding(&song, 3), keys(&[64]), "Off の間の書き換えは記録しない");
}

/// 書き換えた箱をずらしたり、内側に箱を置いて割ったりしても、記録のあるメロディーは今の箱のコードへ合わせて鳴る
/// (書いた音へ戻らない)。新しく覆った (記録の無い) メロディーは動かない。
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
    assert_eq!(sounding(&song, MELODY), keys(&[64, 66, 68, 66, 64, 64, 64, 64]), "内側の E の箱は C → E");
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
    let follow = TrackTranspose::of(&song, MELODY);
    let track = song.track_by_id(MELODY).expect("track");
    let heard = |clip: &Clip| -> Vec<Option<u8>> {
        song.clip_notes(clip)
            .iter()
            .filter(|n| clip.window_has_onset(n.start_beat))
            .map(|n| follow.key(clip, n, Some(clip.content_to_song_beat(n.start_beat))))
            .collect()
    };
    let back = track.clips.iter().find(|c| c.start_beat == 2.0).expect("後ろの片");
    assert_ne!(back.content_id, track.clips.iter().find(|c| c.id == 200).expect("linked").content_id, "fork される");
    assert_eq!(heard(back), keys(&[66, 78]), "後ろの片も C → D");
    assert_eq!(heard(&track.clips[0]), keys(&[66, 78]));
}

/// オーディオ: 和音を弾いていない音源にコードを置いただけでは動かず、書き換えるとその差だけ動く (記録は take の
/// 音源フレーム)。後から同じ音源の event を置いたクリップは、記録が無いので動かない。和音を弾いている区間
/// (`chordal`) があれば、それが基準になる。
#[test]
fn audio_follows_only_rewrites_unless_it_plays_a_chord() {
    use crate::audio_render::TempoMap;
    use crate::model::{AudioChordSpan, AudioContent, AudioEvent, AudioSource, AudioSourcePath};
    // 120 BPM: 1 拍 = 24 000 frame。8 拍の音源を Raw で鳴らす。
    let mut song = Song::default();
    song.media.audio_sources.insert(1, AudioSource {
        path: AudioSourcePath::Absolute("x.wav".into()),
        sample_rate: 48_000,
        channels: 1,
        frames: 192_000,
        original_bpm: None,
        root_key: None,
    });
    let event = AudioEvent {
        id: 1,
        source_id: 1,
        event_length_beats: 8.0,
        source_end_frames: 192_000,
        stretch_mode: crate::model::StretchMode::Raw,
        ..AudioEvent::default()
    };
    let content_id = song.alloc_content_id();
    song.clip_contents.insert(content_id, ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }));
    let clip = Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id, ..Clip::default() };
    song.tracks.push(Track { id: 1, follow_chords: ChordFollow::Parallel, clips: vec![clip], ..Track::default() });
    song.place_chord(boxed(0.0, 4.0, "C"));
    song.place_chord(boxed(4.0, 4.0, "F"));
    let shifts = |song: &Song, clip_idx: usize| -> Vec<i32> {
        let clip = &song.tracks[0].clips[clip_idx];
        let Some(ClipContent::Audio(a)) = song.clip_contents.get(&clip.content_id) else { panic!("audio") };
        let bases = song.audio_event_bases(&TempoMap::from_song(song), clip, &a.events[0]);
        let f = TrackChordFollow::of(song, 1);
        [1.0, 5.0]
            .map(|beat: f64| {
                let at = beat + clip.start_beat;
                let basis = bases.iter().find(|b| b.start_beat <= at && at < b.end_beat).map_or(Basis::NONE, |b| b.basis());
                f.shift(at, basis)
            })
            .to_vec()
    };
    assert_eq!(shifts(&song, 0), vec![0, 0], "置いただけ");
    assert!(song.transpose_chords(&[], 2));
    assert_eq!(shifts(&song, 0), vec![2, 2], "書き換えの差");
    // 書き換えの後に同じ音源で置いたクリップ (別の take の扱いではなく、新しいクリップの記録が無い)。
    let mut later = song.tracks[0].clips[0].clone();
    later.id = 2;
    later.start_beat = 8.0;
    later.chord_basis = ClipChordBasis::default();
    song.tracks[0].clips.push(later);
    song.place_chord(boxed(8.0, 8.0, "E"));
    assert_eq!(shifts(&song, 1), vec![0, 0], "後から置いたクリップは動かない");
    // 素材が Am を弾いている (前半 4 拍): 自分の和音が基準 (Am → D = +5)。後半は記録 (F → G = +2)。
    let span = AudioChordSpan { start_frame: 0, end_frame: 96_000, chord: chord("Am"), chordal: true };
    song.audio_chords.insert(1, vec![span]);
    assert_eq!(shifts(&song, 0), vec![5, 2]);
}

/// 焼き込み (バウンス) は、素材が動いていた元のコードを焼いた音源へ写す: 書き換えで動いたメロディーも、帯のコードへ
/// 合わせて動いていた和音のパートも、焼いた音が同じ量だけ動く。動いていなかった素材は写さない。
#[test]
fn baking_copies_how_the_material_moved() {
    use crate::audio_render::TempoMap;
    use crate::model::{AudioEvent, AudioSource, AudioSourcePath, ClipKey};
    let mut song = piano_and_melody(ChordFollow::Parallel, ChordFollow::Parallel);
    song.place_chord(boxed(0.0, 4.0, "C"));
    let id = song.chords[0].id;
    assert!(song.set_chord(&[id], chord("D")));
    song.place_chord(boxed(4.0, 4.0, "Am"));
    if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&song.tracks[0].clips[0].content_id) {
        m.notes.extend([note(4, 4.0, 4.0, 60), note(5, 4.0, 4.0, 64), note(6, 4.0, 4.0, 67)]);
    }
    // 焼いた音源 (8 拍、120 BPM / 48 kHz の Raw)。
    song.media.audio_sources.insert(9, AudioSource {
        path: AudioSourcePath::Absolute("baked.wav".into()),
        sample_rate: 48_000,
        channels: 2,
        frames: 192_000,
        original_bpm: None,
        root_key: None,
    });
    let event = AudioEvent {
        id: 1,
        source_id: 9,
        event_length_beats: 8.0,
        source_end_frames: 192_000,
        stretch_mode: crate::model::StretchMode::Raw,
        ..AudioEvent::default()
    };
    let baked = Clip { id: 50, start_beat: 0.0, length_beats: 8.0, ..Clip::default() };
    let shifts = |track: u32| -> Vec<i32> {
        let key = ClipKey { track_id: track, clip_id: 100 + track };
        let clip = Clip { chord_basis: song.baked_chord_basis(&[key], &baked, &event), ..baked.clone() };
        let bases = song.audio_event_bases(&TempoMap::from_song(&song), &clip, &event);
        let f = TrackChordFollow::of(&song, track);
        [1.0, 5.0]
            .map(|at: f64| {
                let basis = bases.iter().find(|b| b.start_beat <= at && at < b.end_beat).map_or(Basis::NONE, |b| b.basis());
                f.shift(at, basis)
            })
            .to_vec()
    };
    // メロディー: 前半は書き換え C → D (+2)、後半は置いただけ (0)。
    assert_eq!(shifts(MELODY), vec![2, 0]);
    // ピアノ: 前半は書き換え C → D (+2)、後半の C の和音は Am へ合わせて動いていた (-3)。
    assert_eq!(shifts(PIANO), vec![2, -3]);
}

/// 呼び名だけを変えた書き換え (C6 → Am7、構成音が同じ) ではメロディーは動かない。どのコードでも、書かれたときの
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
                    for p in 0..=127u8 {
                        for key in [None, Some(&key)] {
                            assert_eq!(follow_pitch(mode, p, &c, Basis::placed(&c), key), Some(p), "{mode:?} {c:?} {p}");
                        }
                    }
                    assert_eq!(follow_shift(mode, &c, Basis::placed(&c), None), 0);
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
    let src = |start: f64| NoteSource { own: &own, basis: &[], note_id: 1, start_beat: start };
    assert_eq!(f.key(61, 4.0, &src(4.0)), Some(62));
    assert_eq!(f.key(61, 4.0 - 1e-9, &src(4.0 - 1e-9)), Some(62));
}

/// 範囲外へ出たら鳴らさない (端に丸めない)。
#[test]
fn out_of_range_is_silent() {
    assert_eq!(follow_pitch(ChordFollow::Parallel, 2, &chord("G"), Basis::own(&chord("C")), None), None);
    assert_eq!(follow_pitch(ChordFollow::Parallel, 2, &chord("G"), Basis::placed(&chord("C")), None), None);
}
