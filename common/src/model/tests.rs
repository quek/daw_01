use super::*;

fn json_roundtrip<T>(value: &T) -> T
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let json = serde_json::to_string(value).unwrap();
    serde_json::from_str(&json).unwrap()
}

// ---- Arranger Section model ----

fn mk_section(id: u32, start: f64, len: f64) -> Section {
    Section {
        id,
        name: format!("S{id}"),
        color: [0.5, 0.5, 0.5],
        start_beat: start,
        len_beats: len,
    }
}

#[test]
fn section_end_beat_is_start_plus_len() {
    assert_eq!(mk_section(1, 4.0, 8.0).end_beat(), 12.0);
}

#[test]
fn alloc_section_id_skips_zero_and_increments() {
    let mut song = Song::default();
    assert_eq!(song.alloc_section_id(), 1);
    assert_eq!(song.alloc_section_id(), 2);
    // `0` sentinel が入っていても 1 から採番。
    song.ids.next_section_id = 0;
    assert_eq!(song.alloc_section_id(), 1);
}

#[test]
fn normalize_sections_sorts_disjoint_by_start() {
    let mut song = Song {
        sections: vec![
            mk_section(1, 8.0, 4.0),
            mk_section(2, 0.0, 4.0),
            mk_section(3, 4.0, 4.0),
        ],
        ..Default::default()
    };
    song.normalize_sections();
    let ids: Vec<u32> = song.sections.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![2, 3, 1]);
    assert_eq!(song.sections[0].start_beat, 0.0);
    assert_eq!(song.sections[0].len_beats, 4.0);
}

#[test]
fn normalize_sections_resolves_overlap_by_clamping_later_start() {
    // [0,6) と [4,8) が重複 → 後発の start を直前 end(6) までクランプ → [6,8)。
    let mut song = Song {
        sections: vec![mk_section(1, 0.0, 6.0), mk_section(2, 4.0, 4.0)],
        ..Default::default()
    };
    song.normalize_sections();
    assert_eq!(song.sections.len(), 2);
    assert_eq!(
        (song.sections[0].start_beat, song.sections[0].len_beats),
        (0.0, 6.0)
    );
    assert_eq!(
        (song.sections[1].start_beat, song.sections[1].len_beats),
        (6.0, 2.0)
    );
}

#[test]
fn normalize_sections_drops_zero_negative_and_fully_overlapped() {
    let mut song = Song {
        sections: vec![
            mk_section(1, 0.0, 4.0),
            mk_section(2, 4.0, 0.0),
            mk_section(3, 8.0, -1.0),
        ],
        ..Default::default()
    };
    song.normalize_sections();
    assert_eq!(
        song.sections.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![1]
    );

    // [0,10) が [2,4) を完全に覆う → 後発は len 0 になり drop。
    let mut song = Song {
        sections: vec![mk_section(1, 0.0, 10.0), mk_section(2, 2.0, 2.0)],
        ..Default::default()
    };
    song.normalize_sections();
    assert_eq!(song.sections.len(), 1);
    assert_eq!(song.sections[0].id, 1);
}

#[test]
fn section_survives_json_roundtrip() {
    let s = mk_section(7, 12.0, 4.0);
    assert_eq!(json_roundtrip(&s), s);
}

#[test]
fn ripple_timeline_open_shifts_everything_after_from_beat_right() {
    let mut song = Song {
        length_beats: 16.0,
        sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 8.0, 4.0)],
        tracks: vec![Track {
            clips: vec![
                Clip {
                    start_beat: 0.0,
                    length_beats: 4.0,
                    ..Default::default()
                },
                Clip {
                    start_beat: 8.0,
                    length_beats: 4.0,
                    ..Default::default()
                },
            ],
            ..Track::default()
        }],
        ..Default::default()
    };

    // beat 4 に 4 拍挿入 (open) → `>= 4` が +4。
    let r = song.ripple_timeline(4.0, 4.0);

    assert_eq!(song.tracks[0].clips[0].start_beat, 0.0); // 4 未満は不変
    assert_eq!(song.tracks[0].clips[1].start_beat, 12.0); // 8 → 12
    assert_eq!(song.sections[0].start_beat, 0.0);
    assert_eq!(song.sections[1].start_beat, 12.0);
    assert_eq!(song.length_beats, 20.0);
    // 返された Ripple は Song の外に住む時間位置 (ループ範囲) にも同じ規則で効く。
    let mut region = LoopRegion { enabled: true, start_beat: 8.0, end_beat: 12.0 };
    region.apply_ripple(r);
    assert_eq!((region.start_beat, region.end_beat), (12.0, 16.0));
}

#[test]
fn move_section_reorders_content_with_ripple() {
    let mut song = Song {
        length_beats: 12.0,
        sections: vec![
            mk_section(1, 0.0, 4.0),
            mk_section(2, 4.0, 4.0),
            mk_section(3, 8.0, 4.0),
        ],
        tracks: vec![Track {
            id: 1,
            clips: vec![
                Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 4.0,
                    ..Default::default()
                }, // Intro
                Clip {
                    id: 2,
                    start_beat: 4.0,
                    length_beats: 4.0,
                    ..Default::default()
                }, // Verse
                Clip {
                    id: 3,
                    start_beat: 8.0,
                    length_beats: 4.0,
                    ..Default::default()
                }, // Chorus
            ],
            ..Track::default()
        }],
        ..Default::default()
    };

    // Chorus ([8,12), id=3) を Verse の前 (dest=4) へ移動。
    assert!(!song.move_section(3, 4.0).is_empty());

    // セクションは Intro[0,4) / Chorus[4,8) / Verse[8,12) に組み替わる (start 昇順)。
    let secs: Vec<(u32, f64, f64)> = song
        .sections
        .iter()
        .map(|s| (s.id, s.start_beat, s.len_beats))
        .collect();
    assert_eq!(secs, vec![(1, 0.0, 4.0), (3, 4.0, 4.0), (2, 8.0, 4.0)]);

    // clip も帯に追従: clip1→0, clip3→4, clip2→8。
    let mut clips: Vec<(u32, f64)> = song.tracks[0]
        .clips
        .iter()
        .map(|c| (c.id, c.start_beat))
        .collect();
    clips.sort_by_key(|c| c.0);
    assert_eq!(clips, vec![(1, 0.0), (2, 8.0), (3, 4.0)]);
}

/// r.md #71 (ユーザー報告そのもの): 「Break を 2 まで D&D しても元に戻ってしまいます。
/// その先の C まで D&D すると Break と 2 が入れかわります。」
///
/// 帯を **右へ** 動かすときだけ、落とし先が 1 セクションぶん先にずれていた。
/// 原因は `move_section` が `dest_start` を「[a,b) を ripple-close した **中間座標系**」 の
/// 絶対拍として解釈していたこと (`dest_start - len` / 自分の元範囲内なら元の位置へ)。
/// ユーザーが指しているのは画面で見えている位置 = **移動後の座標系**なので、
/// 逆算を呼び出し側に強いる形が誤りだった。
#[test]
fn move_section_forward_swaps_with_the_band_under_the_cursor() {
    let mk = |sections: Vec<Section>| Song {
        length_beats: 12.0,
        sections,
        tracks: vec![Track {
            id: 1,
            clips: vec![
                Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() },
                Clip { id: 2, start_beat: 4.0, length_beats: 4.0, ..Default::default() },
                Clip { id: 3, start_beat: 8.0, length_beats: 4.0, ..Default::default() },
            ],
            ..Track::default()
        }],
        ..Default::default()
    };
    let secs = |song: &Song| -> Vec<(u32, f64, f64)> {
        song.sections.iter().map(|s| (s.id, s.start_beat, s.len_beats)).collect()
    };
    let clips = |song: &Song| -> Vec<(u32, f64)> {
        let mut v: Vec<(u32, f64)> =
            song.tracks[0].clips.iter().map(|c| (c.id, c.start_beat)).collect();
        v.sort_by_key(|c| c.0);
        v
    };

    // Break[0,4) / 2[4,8) / C[8,12)。Break を「2 の場所」 (拍 4) へ落とす。
    let mut song = mk(vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0), mk_section(3, 8.0, 4.0)]);
    assert!(!song.move_section(1, 4.0).is_empty(), "移動が起きる (元に戻らない)");
    assert_eq!(
        secs(&song),
        vec![(2, 0.0, 4.0), (1, 4.0, 4.0), (3, 8.0, 4.0)],
        "Break と 2 が入れかわる (C は動かない)"
    );
    assert_eq!(clips(&song), vec![(1, 4.0), (2, 0.0), (3, 8.0)], "clip も帯に追従する");

    // その先の C (拍 8) へ落としたら **C と入れかわる** (= 1 つ先まで飛ばない)。
    let mut song = mk(vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0), mk_section(3, 8.0, 4.0)]);
    assert!(!song.move_section(1, 8.0).is_empty());
    assert_eq!(
        secs(&song),
        vec![(2, 0.0, 4.0), (3, 4.0, 4.0), (1, 8.0, 4.0)],
        "Break が末尾へ回り、2 と C が前へ詰まる"
    );
    assert_eq!(clips(&song), vec![(1, 8.0), (2, 0.0), (3, 4.0)]);
}

/// 落とし先の拍は **移動後の座標系での帯の開始位置** = ドラッグ中に見えていた位置。
/// 前へ動かす場合は元から正しかったので、その挙動が変わっていないことも固定する
/// (`move_section_reorders_content_with_ripple` の後方移動と対になる前方移動の確認)。
#[test]
fn move_section_lands_where_the_preview_showed_it() {
    for (id, dest, want_start) in [(1_u32, 4.0_f64, 4.0_f64), (1, 8.0, 8.0), (3, 0.0, 0.0), (3, 4.0, 4.0)] {
        let mut song = Song {
            length_beats: 12.0,
            sections: vec![
                mk_section(1, 0.0, 4.0),
                mk_section(2, 4.0, 4.0),
                mk_section(3, 8.0, 4.0),
            ],
            ..Default::default()
        };
        assert!(!song.move_section(id, dest).is_empty(), "id={id} dest={dest}");
        let got = song.sections.iter().find(|s| s.id == id).map(|s| s.start_beat);
        assert_eq!(
            got,
            Some(want_start),
            "id={id} を dest={dest} へ落としたら開始拍は {want_start} (見えていた位置)"
        );
    }
}

/// r.md #71: 他帯に **食い込む** 位置を指したら、近い方の境界へ寄せて着地する。
///
/// 寄せないと最後の `normalize_sections` が重なりを潰し、**帯の長さが変わって**しまう
/// (ドラッグ中に見えていた帯と別物が出来る = overlay ≠ commit)。
/// 合法な位置は素通しなので、通常のドラッグの感触は変わらない。
#[test]
fn move_section_resolves_dest_that_would_overlap_another_band() {
    // Break[0,4) / 2[4,8) / C[8,12)。Break を抜くと 2→[0,4) / C→[4,8) に詰まる。
    let mk = || Song {
        length_beats: 12.0,
        sections: vec![
            mk_section(1, 0.0, 4.0),
            mk_section(2, 4.0, 4.0),
            mk_section(3, 8.0, 4.0),
        ],
        ..Default::default()
    };
    // 落とし先の拍 → 着地する開始拍。境界 (0/4/8) は素通し、内側は近い端へ。
    for (desired, want) in [(4.0_f64, 4.0_f64), (8.0, 8.0), (6.0, 4.0), (7.0, 8.0), (1.0, 0.0), (3.0, 4.0)] {
        let mut song = mk();
        song.move_section(1, desired);
        let moved = song.sections.iter().find(|s| s.id == 1).expect("帯は消えない");
        assert!(
            (moved.start_beat - want).abs() < 1e-9,
            "dest={desired} → 開始拍 {want} へ寄る: got {}",
            moved.start_beat
        );
        assert!(
            (moved.len_beats - 4.0).abs() < 1e-9,
            "帯の長さは変わらない (潰されない): dest={desired} got {}",
            moved.len_beats
        );
        // 帯どうしは重なっていない (`normalize_sections` の invariant を実際に満たす)。
        let mut spans: Vec<(f64, f64)> =
            song.sections.iter().map(|s| (s.start_beat, s.end_beat())).collect();
        spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0 + 1e-9, "重なりが残った: {spans:?} (dest={desired})");
        }
    }
}

/// `resolve_section_move_dest` は **冪等**。 preview 側で解決した値をそのまま
/// `move_section` に渡しても二重補正にならない (合法な位置は素通し) ことの明示。
#[test]
fn resolve_section_move_dest_is_idempotent() {
    let others = [(4.0_f64, 4.0_f64), (8.0, 4.0)];
    for desired in [0.0_f64, 1.0, 3.0, 4.0, 6.0, 7.0, 8.0, 20.0] {
        let once = resolve_section_move_dest(others.iter().copied(), 0.0, 4.0, desired);
        let twice = resolve_section_move_dest(others.iter().copied(), 0.0, 4.0, once);
        assert!((once - twice).abs() < 1e-9, "desired={desired}: {once} -> {twice}");
    }
}

/// r.md #71 同件: Ctrl+drag の **複製** も、他帯に食い込む位置へ落とすと
/// `normalize_sections` に潰されて **複製が短くなる** (ゴーストは満寸で見えているのに)。
/// 複製は close を伴わないので障害物は「全帯を現在位置のまま」 = 元帯も含む。
#[test]
fn duplicate_section_resolves_dest_that_would_overlap_another_band() {
    let mk = || Song {
        length_beats: 8.0,
        sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0)],
        // 既存 id 1,2 の続き (複製の id が既存と衝突しないように)。
        ids: IdAllocators { next_section_id: 3, ..Song::default().ids },
        ..Default::default()
    };
    // 落とし先の拍 → 着地する開始拍。境界 (0/4/8) は素通し、2[4,8) の内側は近い端へ
    // (ちょうど中点の 6 は同着なので手前 = insert-before 寄り)。
    for (desired, want) in [(4.0_f64, 4.0_f64), (8.0, 8.0), (5.0, 4.0), (6.0, 4.0), (7.0, 8.0)] {
        let mut song = mk();
        let (new_id, _) = song.duplicate_section(1, desired).expect("複製される");
        let dup = song.sections.iter().find(|s| s.id == new_id).expect("複製された帯");
        assert!(
            (dup.start_beat - want).abs() < 1e-9,
            "dest={desired} → 開始拍 {want}: got {}",
            dup.start_beat
        );
        assert!(
            (dup.len_beats - 4.0).abs() < 1e-9,
            "長さは保たれる (潰されない): dest={desired} got {}",
            dup.len_beats
        );
        // 元帯も潰されていない。
        let orig = song.sections.iter().find(|s| s.id == 1).expect("元帯");
        assert!((orig.len_beats - 4.0).abs() < 1e-9, "元帯の長さも不変: got {}", orig.len_beats);
    }
}

#[test]
fn duplicate_section_inserts_linked_copy_with_ripple() {
    let mut song = Song {
        length_beats: 8.0,
        sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0)],
        // 既存 id 1,2 の続き (実運用は alloc 経由で採番される)
        ids: IdAllocators {
            next_section_id: 3,
            ..Song::default().ids
        },
        tracks: vec![Track {
            id: 1,
            clips: vec![
                Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 4.0,
                    content_id: 7,
                    ..Default::default()
                },
                Clip {
                    id: 2,
                    start_beat: 4.0,
                    length_beats: 4.0,
                    content_id: 8,
                    ..Default::default()
                },
            ],
            next_clip_id: 3,
            ..Track::default()
        }],
        ..Default::default()
    };

    // section1 ([0,4), content_id 7) を 2 つの間 (dest=4) に複製。
    let (new_id, _ripple) = song.duplicate_section(1, 4.0).unwrap();
    assert_eq!(new_id, 3);

    // section1[0,4) / copy[4,8) / section2[8,12) の 3 つ。
    assert_eq!(song.sections.len(), 3);
    let mut starts: Vec<f64> = song.sections.iter().map(|s| s.start_beat).collect();
    starts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(starts, vec![0.0, 4.0, 8.0]);

    // clip: 元 clip1(0,c7) / 複製(4,c7 linked) / 元 clip2 は 4→8 へ ripple(8,c8)。
    let mut clips: Vec<(f64, u32)> = song.tracks[0]
        .clips
        .iter()
        .map(|c| (c.start_beat, c.content_id))
        .collect();
    clips.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    assert_eq!(clips, vec![(0.0, 7), (4.0, 7), (8.0, 8)]);
    // 複製は新しい clip id (>= 3)。
    assert!(
        song.tracks[0]
            .clips
            .iter()
            .any(|c| c.start_beat == 4.0 && c.id >= 3)
    );
}

#[test]
fn split_clips_at_forks_straddling_clip_content() {
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Midi(MidiContent {
            notes: vec![
                Note {
                    id: 1,
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                },
                Note {
                    id: 2,
                    start_beat: 3.0,
                    duration_beats: 1.0,
                    pitch: 62,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                },
            ],
            next_note_id: 3,
        }),
    );
    let track = Track {
        id: 1,
        clips: vec![Clip {
            id: 1,
            start_beat: 2.0,
            length_beats: 4.0,
            content_id: cid,
            ..Default::default()
        }],
        next_clip_id: 2,
        ..Track::default()
    };
    song.tracks = vec![track];

    // clip [2,6) を beat 4 (clip-local 2.0) で分割。
    song.split_clips_at(4.0);

    let mut cs: Vec<(f64, f64, u32)> = song.tracks[0]
        .clips
        .iter()
        .map(|c| (c.start_beat, c.length_beats, c.content_id))
        .collect();
    cs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    assert_eq!(cs.len(), 2);
    // 左 [2,4) は元 content を保持、 右 [4,6) は fork content。
    assert_eq!((cs[0].0, cs[0].1, cs[0].2), (2.0, 2.0, cid));
    assert_eq!((cs[1].0, cs[1].1), (4.0, 2.0));
    let right_cid = cs[1].2;
    assert_ne!(right_cid, cid);

    // 右 content の note は cut(2.0) 左シフト (3.0→1.0)、 0.0 の note は drop。
    let ClipContent::Midi(m) = &song.clip_contents[&right_cid] else {
        panic!("midi")
    };
    assert_eq!(m.notes.len(), 1);
    assert_eq!(m.notes[0].start_beat, 1.0);
    assert_eq!(m.notes[0].pitch, 62);

    // 元 content は pooled で不変 (2 note のまま)。
    let ClipContent::Midi(orig) = &song.clip_contents[&cid] else {
        panic!("midi")
    };
    assert_eq!(orig.notes.len(), 2);
}

#[test]
fn delete_section_removes_band_only_keeping_content() {
    let mut song = Song {
        sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0)],
        tracks: vec![Track {
            id: 1,
            clips: vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                ..Default::default()
            }],
            ..Track::default()
        }],
        ..Default::default()
    };
    assert!(song.delete_section(1));
    assert_eq!(
        song.sections.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(song.tracks[0].clips.len(), 1); // 内容は温存
    assert!(!song.delete_section(999)); // 不在は false
}

#[test]
fn delete_section_range_removes_content_and_ripples() {
    let mut song = Song {
        length_beats: 12.0,
        sections: vec![
            mk_section(1, 0.0, 4.0),
            mk_section(2, 4.0, 4.0),
            mk_section(3, 8.0, 4.0),
        ],
        tracks: vec![Track {
            id: 1,
            clips: vec![
                Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 4.0,
                    ..Default::default()
                },
                Clip {
                    id: 2,
                    start_beat: 4.0,
                    length_beats: 4.0,
                    ..Default::default()
                },
                Clip {
                    id: 3,
                    start_beat: 8.0,
                    length_beats: 4.0,
                    ..Default::default()
                },
            ],
            ..Track::default()
        }],
        ..Default::default()
    };
    // 真ん中 section2 [4,8) を範囲ごと削除 → clip2 消滅、 clip3 と section3 が 8→4 へ詰まる。
    assert!(song.delete_section_range(2).is_some());
    let mut secs: Vec<(u32, f64)> = song.sections.iter().map(|s| (s.id, s.start_beat)).collect();
    secs.sort_by_key(|s| s.0);
    assert_eq!(secs, vec![(1, 0.0), (3, 4.0)]);
    let mut clips: Vec<(u32, f64)> = song.tracks[0]
        .clips
        .iter()
        .map(|c| (c.id, c.start_beat))
        .collect();
    clips.sort_by_key(|c| c.0);
    assert_eq!(clips, vec![(1, 0.0), (3, 4.0)]);
    assert_eq!(song.length_beats, 8.0);
}

#[test]
fn move_section_noop_when_dest_equals_start() {
    let mut song = Song {
        sections: vec![mk_section(1, 0.0, 4.0)],
        ..Default::default()
    };
    assert!(song.move_section(1, 0.0).is_empty());
    assert!(song.move_section(99, 8.0).is_empty()); // 存在しない id
}

#[test]
fn ripple_timeline_close_shifts_left_and_shrinks_length() {
    let mut song = Song {
        length_beats: 16.0,
        tracks: vec![Track {
            clips: vec![Clip {
                start_beat: 12.0,
                length_beats: 4.0,
                ..Default::default()
            }],
            ..Track::default()
        }],
        ..Default::default()
    };

    // beat 8 以降を 4 拍詰める (close) → `>= 8` が -4。
    song.ripple_timeline(8.0, -4.0);

    assert_eq!(song.tracks[0].clips[0].start_beat, 8.0); // 12 → 8
    assert_eq!(song.length_beats, 12.0);
}

#[test]
fn ensure_image_video_event_coverage_extends_short_event() {
    // clip=48 / image event=32 → event を 48 まで extend して
    // clip 範囲内で途中消失しないようにする (= 既存 .daw の load 修復)。
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Image(ImageContent {
            events: vec![ImageEvent {
                event_start_in_clip_beats: 0.0,
                event_length_beats: 32.0,
                ..ImageEvent::default()
            }],
        }),
    );
    let tid = song.alloc_track_id();
    let mut track = Track {
        id: tid,
        ..Track::default()
    };
    let clip_id = track.alloc_clip_id();
    track.clips.push(Clip {
        id: clip_id,
        start_beat: 0.0,
        length_beats: 48.0,
        content_id: cid,
        ..Clip::default()
    });
    song.tracks.push(track);

    song.ensure_overlay_event_coverage();
    let ClipContent::Image(c) = &song.clip_contents[&cid] else {
        panic!("expected image content");
    };
    assert_eq!(c.events[0].event_length_beats, 48.0);

    // idempotent: 2 回目で値は変わらない。
    song.ensure_overlay_event_coverage();
    let ClipContent::Image(c) = &song.clip_contents[&cid] else {
        panic!("expected image content");
    };
    assert_eq!(c.events[0].event_length_beats, 48.0);
}

#[test]
fn ensure_image_video_event_coverage_extend_only_across_linked_clips() {
    // 同 content を len 8 と len 48 の 2 clip が共有 → 最長 48 まで extend。
    // 短い clip は自分の clip 範囲 gate で clamp されるので event は縮めない。
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Image(ImageContent {
            events: vec![ImageEvent {
                event_start_in_clip_beats: 0.0,
                event_length_beats: 4.0,
                ..ImageEvent::default()
            }],
        }),
    );
    let tid = song.alloc_track_id();
    let mut track = Track {
        id: tid,
        ..Track::default()
    };
    for (start, len) in [(0.0_f64, 8.0_f64), (16.0, 48.0)] {
        let clip_id = track.alloc_clip_id();
        track.clips.push(Clip {
            id: clip_id,
            start_beat: start,
            length_beats: len,
            content_id: cid,
            ..Clip::default()
        });
    }
    song.tracks.push(track);

    song.ensure_overlay_event_coverage();
    let ClipContent::Image(c) = &song.clip_contents[&cid] else {
        panic!("expected image content");
    };
    assert_eq!(c.events[0].event_length_beats, 48.0);
}

#[test]
fn ensure_overlay_event_coverage_extends_text_clip() {
    // (Text 版): クレジット text clip @0+48 だが event_length=4 →
    // bar2 (beat4) で event 範囲を抜けて消える。event を 48 まで extend する。
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Text(TextContent {
            events: vec![TextEvent {
                text: "クレジット".into(),
                event_start_in_clip_beats: 0.0,
                event_length_beats: 4.0,
                ..TextEvent::default()
            }],
        }),
    );
    let tid = song.alloc_track_id();
    let mut track = Track {
        id: tid,
        ..Track::default()
    };
    let clip_id = track.alloc_clip_id();
    track.clips.push(Clip {
        id: clip_id,
        start_beat: 0.0,
        length_beats: 48.0,
        content_id: cid,
        ..Clip::default()
    });
    song.tracks.push(track);

    song.ensure_overlay_event_coverage();
    let ClipContent::Text(c) = &song.clip_contents[&cid] else {
        panic!("expected text content");
    };
    assert_eq!(c.events[0].event_length_beats, 48.0);
}

#[test]
fn song_default_roundtrip() {
    let song = Song::default();
    assert_eq!(json_roundtrip(&song), song);
}

/// `track_visually_silenced` は audio engine の effective-mute と同じ意味論を
/// video 層に再現する: グループ親の mute は subtree を隠し、 solo は audio と
/// 一致 (グループを solo すると非 solo の子は隠れる / 子を solo すると親 group
/// は見える)。
#[test]
fn track_visually_silenced_mute_and_solo_semantics() {
    // t10 = group, t11 = t10 の子, t12 = 独立。
    let mk = |id: u32, parent: Option<u32>| Track {
        id,
        parent_group_id: parent,
        ..Track::default()
    };
    let mut song = Song {
        tracks: vec![mk(10, None), mk(11, Some(10)), mk(12, None)],
        ..Default::default()
    };

    // baseline: 何も silenced でない。
    assert!(!song.track_visually_silenced(11));
    assert!(!song.track_visually_silenced(12));

    // グループ (祖先) の mute が子を隠す。
    song.track_by_id_mut(10).unwrap().muted = true;
    assert!(
        song.track_visually_silenced(11),
        "child of muted group hidden"
    );
    assert!(
        !song.track_visually_silenced(12),
        "unrelated track unaffected"
    );
    song.track_by_id_mut(10).unwrap().muted = false;

    // 自身の mute。
    song.track_by_id_mut(12).unwrap().muted = true;
    assert!(song.track_visually_silenced(12));
    song.track_by_id_mut(12).unwrap().muted = false;

    // leaf を solo: それだけ可視、 他は隠れる。
    song.track_by_id_mut(12).unwrap().solo = true;
    assert!(!song.track_visually_silenced(12), "soloed track visible");
    assert!(
        song.track_visually_silenced(11),
        "non-soloed hidden under solo"
    );
    song.track_by_id_mut(12).unwrap().solo = false;

    // グループを solo: 配下の子も可視 (folder solo、 Ableton/Reaper 準拠)。
    song.track_by_id_mut(10).unwrap().solo = true;
    assert!(
        !song.track_visually_silenced(10),
        "soloed group itself visible"
    );
    assert!(
        !song.track_visually_silenced(11),
        "child of soloed group visible (folder solo)"
    );
    assert!(
        song.track_visually_silenced(12),
        "unrelated hidden under solo"
    );
    assert!(song.ancestor_soloed(11), "child sees soloed ancestor group");
    assert!(
        !song.ancestor_soloed(12),
        "unrelated has no soloed ancestor"
    );
    song.track_by_id_mut(10).unwrap().solo = false;

    // 子を solo: その祖先 group は可視のまま (has_soloed_contributor 相当)。
    song.track_by_id_mut(11).unwrap().solo = true;
    assert!(!song.track_visually_silenced(11), "soloed child visible");
    assert!(
        !song.track_visually_silenced(10),
        "ancestor of soloed child visible"
    );
    assert!(
        song.track_visually_silenced(12),
        "unrelated hidden under solo"
    );
}

/// Regression test for sidechain pipeline: when `ensure_ids()` rewrites
/// a `track.id == 0` sentinel into a fresh id, every reference to that
/// old id (= `aux_inputs` tap source + `parent_group_id`) must be
/// remapped too. Otherwise the references dangle, `compile_schedule`
/// silently skips them (treating dangling sidechain sources as
/// `continue`), and the user sees no sidechain signal even though the
/// dropdown is wired correctly.
///
/// Setup:
///   Track Kick id=0 (sentinel) → after ensure_ids gets id=2
///   Track Bass id=1 with fx[0].aux_inputs=[post_fader(0)] (= Kick)
///                    parent_group_id = Some(0) (= Kick)
/// Expected after ensure_ids:
///   Bass.fx[0].aux_inputs == [post_fader(2)]
///   Bass.parent_group_id == Some(2)
#[test]
fn ensure_ids_remaps_aux_inputs_and_parent_group_id() {
    use crate::plugin_format::PluginFormat;

    let mut song = Song {
        bpm: 120.0,
        time_sig: (4, 4),
        length_beats: 64.0,
        tracks: vec![
            Track {
                id: 0, // sentinel — will be replaced by ensure_ids
                name: "Kick".into(),
                ..Track::default()
            },
            Track {
                id: 1,
                name: "Bass".into(),
                parent_group_id: Some(0), // points at Kick's old sentinel id
                // v23: 役割別 chain は廃止。device chain (`devices`) に直接置く。
                devices: vec![PluginInstance {
                    // points at Kick (sentinel id 0)
                    aux_inputs: vec![Some(AuxInputRoute::post_fader(0))],
                    ..PluginInstance::with_ports(
                        "test.compressor".into(),
                        PluginFormat::Vst3,
                        crate::port_config::PortConfig::default(),
                    )
                }],
                ..Track::default()
            },
        ],
        ids: IdAllocators {
            next_track_id: 2,
            ..Song::default().ids
        },
        ..Song::default()
    };

    song.ensure_ids();

    // Kick got rebased.
    let kick = &song.tracks[0];
    assert_ne!(kick.id, 0, "ensure_ids should replace sentinel id 0");
    let new_kick_id = kick.id;

    // Bass kept its id but its references must be remapped.
    let bass = &song.tracks[1];
    assert_eq!(bass.id, 1);
    assert_eq!(
        bass.parent_group_id,
        Some(new_kick_id),
        "parent_group_id pointing at sentinel must be remapped to the new id"
    );
    assert_eq!(
        bass.devices[0].aux_inputs,
        vec![Some(AuxInputRoute::post_fader(new_kick_id))],
        "aux_inputs tap pointing at sentinel must be remapped to the new id"
    );
}

/// パラアウト (docs/plan_paraout.md): a plugin's `aux_outputs` destination
/// pointing at a sentinel id (0) must be remapped by `ensure_ids` to the
/// child's freshly assigned id — the symmetric counterpart of the
/// `aux_inputs` remap above. Without this, a saved project's parallel-out
/// routing breaks the moment ids are rebased on load.
#[test]
fn ensure_ids_remaps_aux_outputs_dest() {
    use crate::plugin_format::PluginFormat;

    let mut song = Song {
        bpm: 120.0,
        time_sig: (4, 4),
        length_beats: 64.0,
        tracks: vec![
            Track {
                id: 0, // sentinel — becomes the new child id
                name: "Snare".into(),
                ..Track::default()
            },
            Track {
                id: 1,
                name: "Drums".into(),
                devices: vec![PluginInstance {
                    // aux output routed at Snare (sentinel id 0)
                    aux_outputs: vec![Some(AuxOutputRoute::to_track(0))],
                    aux_output_count: 1,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        crate::port_config::PortConfig::default(),
                    )
                }],
                ..Track::default()
            },
        ],
        ids: IdAllocators {
            next_track_id: 2,
            ..Song::default().ids
        },
        ..Song::default()
    };

    song.ensure_ids();

    let snare_id = song.tracks[0].id;
    assert_ne!(snare_id, 0, "ensure_ids should replace sentinel id 0");
    assert_eq!(
        song.tracks[1].devices[0].aux_outputs,
        vec![Some(AuxOutputRoute::to_track(snare_id))],
        "aux_outputs dest pointing at sentinel must be remapped to the new id"
    );
}

/// パラアウト (docs/plan_paraout.md): aux_outputs / aux_output_count は JSON
/// (セーブファイル) と bincode (IPC) の両方で往復しても保持される。 None
/// (未振分けポート) を挟んだ疎なルートも壊れないこと。
#[test]
fn plugin_instance_aux_outputs_survive_json_and_bincode_round_trip() {
    use crate::plugin_format::PluginFormat;

    let inst = PluginInstance {
        aux_outputs: vec![
            Some(AuxOutputRoute::to_track(7)),
            None,
            Some(AuxOutputRoute::to_track(9)),
        ],
        aux_output_count: 3,
        ..PluginInstance::new("test.drum_sampler".into(), PluginFormat::Clap)
    };

    // JSON (save file)
    let via_json = json_roundtrip(&inst);
    assert_eq!(via_json.aux_outputs, inst.aux_outputs);
    assert_eq!(via_json.aux_output_count, 3);

    // bincode (IPC)
    let cfg = bincode::config::standard();
    let bytes = bincode::encode_to_vec(&inst, cfg).unwrap();
    let (via_bincode, _): (PluginInstance, usize) =
        bincode::decode_from_slice(&bytes, cfg).unwrap();
    assert_eq!(via_bincode.aux_outputs, inst.aux_outputs);
    assert_eq!(via_bincode.aux_output_count, 3);
}

/// パラアウト (docs/plan_paraout.md): aux_outputs / aux_output_count フィールドを
/// 持たない旧セーブファイルは `#[serde(default)]` で空にフォワード migrate
/// される (機能追加前の .daw が壊れない)。
#[test]
fn plugin_instance_without_aux_output_fields_forward_migrates_to_empty() {
    use crate::plugin_format::PluginFormat;

    // aux_outputs を持つ instance を JSON 化 → aux_output 系キーを削除して
    // 旧形式を模し → deserialize で空に migrate されることを確認。
    let inst = PluginInstance {
        aux_outputs: vec![Some(AuxOutputRoute::to_track(7))],
        aux_output_count: 2,
        ..PluginInstance::new("test.drum_sampler".into(), PluginFormat::Clap)
    };
    let mut v = serde_json::to_value(&inst).unwrap();
    let obj = v.as_object_mut().unwrap();
    assert!(
        obj.contains_key("aux_outputs"),
        "non-empty aux_outputs must serialize (skip_serializing_if guards only the empty case)"
    );
    obj.remove("aux_outputs");
    obj.remove("aux_output_count");

    let migrated: PluginInstance = serde_json::from_value(v).unwrap();
    assert!(
        migrated.aux_outputs.is_empty(),
        "missing aux_outputs field must forward-migrate to empty"
    );
    assert_eq!(
        migrated.aux_output_count, 0,
        "missing aux_output_count field must forward-migrate to 0"
    );
}

/// docs/plan_modulation.md §8: `mod_sources` get stable ids assigned
/// (sentinel 0 → fresh) and their `tap.source_track` follows a track id
/// remap, exactly like `aux_inputs` taps.
#[test]
fn ensure_ids_assigns_mod_source_ids_and_remaps_tap() {
    let mut song = Song {
        tracks: vec![
            Track {
                id: 0, // sentinel — rebased by ensure_ids
                name: "Kick".into(),
                ..Track::default()
            },
            Track {
                id: 1,
                name: "Bass".into(),
                ..Track::default()
            },
        ],
        ids: IdAllocators {
            next_track_id: 2,
            ..Song::default().ids
        },
        mod_sources: vec![ModSource {
            id: 0, // sentinel — assigned by ensure_ids
            owner_track_id: 0,
            color: ModSource::palette_color(0),
            kind: ModSourceKind::EnvelopeFollower {
                tap: AudioTap::post_fader(0), // points at Kick's sentinel id
                follower: FollowerConfig::default(),
            },
        }],
        ..Song::default()
    };

    song.ensure_ids();

    let new_kick_id = song.tracks[0].id;
    assert_ne!(new_kick_id, 0, "Kick sentinel rebased");
    assert_ne!(song.mod_sources[0].id, 0, "mod_source id assigned");
    assert_eq!(
        song.mod_sources[0].follower().unwrap().0.source_track,
        new_kick_id,
        "mod_source tap.source_track must follow the track id remap"
    );
}

/// `ensure_ids` はレーンの legacy positional `device_index` (= load 時 JSON 前処理
/// `migrate_legacy_device_chains` が flatten / slot 解決後に残す値) を、その device に
/// 採番された安定 `device_id` へ写像する。旧 3-split 平坦化と slot→index は前処理層 (project.rs)
/// のテストで担保する。
#[test]
fn ensure_ids_remaps_lane_device_index_to_device_id() {
    use crate::plugin_format::PluginFormat;
    let plug = |id: &str| PluginInstance::new(id.into(), PluginFormat::Clap);
    // 5-device chain; lane が index 2 (synth) を positional index で指す。
    let mut song = Song {
        tracks: vec![Track {
            id: 1,
            devices: vec![
                plug("arp"),
                plug("quant"),
                plug("synth"),
                plug("comp"),
                plug("reverb"),
            ],
            automation_lanes: vec![AutomationLane {
                id: 1,
                ..AutomationLane::new(
                    AutomationTarget::PluginParam {
                        device_id: 0,
                        param_id: 5,
                        legacy_device_index: Some(2),
                    },
                    0.0,
                )
            }],
            next_lane_id: 2,
            ..Track::default()
        }],
        ids: IdAllocators {
            next_track_id: 2,
            ..Song::default().ids
        },
        ..Song::default()
    };
    song.ensure_ids();

    let t = &song.tracks[0];
    assert_ne!(t.devices[2].id, 0, "devices は安定 id を採番される");
    match t.lane_by_id(1).unwrap().target {
        AutomationTarget::PluginParam {
            device_id,
            legacy_device_index,
            ..
        } => {
            assert_eq!(device_id, t.devices[2].id, "index 2 → devices[2].id");
            assert!(
                legacy_device_index.is_none(),
                "legacy_device_index は消費される"
            );
        }
        _ => panic!("expected PluginParam"),
    }
}

/// r.md #71 (プラグインのコピー / 移動): device をコピーする経路が
/// `alloc_device_id()` を呼び忘れると 2 device が同じ id を共有し、plugin host の
/// dedup (同 device_id + 同 plugin_id) が 1 instance へ silent に merge する
/// (音は出るので気付けない)。 防御は SSoT (`ensure_ids`) 側に置く。
#[test]
fn ensure_ids_reallocates_duplicate_device_ids() {
    use crate::plugin_format::PluginFormat;
    let plug = |id: &str, dev_id: u64| PluginInstance {
        id: dev_id,
        ..PluginInstance::new(id.into(), PluginFormat::Clap)
    };
    let mut song = Song {
        tracks: vec![
            Track {
                id: 1,
                devices: vec![plug("comp", 7)],
                ..Track::default()
            },
            Track {
                id: 2,
                // track 1 の device と **同じ id**。
                devices: vec![plug("comp", 7)],
                ..Track::default()
            },
        ],
        // master_fx_chain にも同 id を置いて、chain をまたいだ重複も見ることを固定する。
        master_fx_chain: vec![plug("limiter", 7)],
        ids: IdAllocators {
            next_track_id: 3,
            ..Song::default().ids
        },
        ..Song::default()
    };
    song.ensure_ids();

    let a = song.tracks[0].devices[0].id;
    let b = song.tracks[1].devices[0].id;
    let m = song.master_fx_chain[0].id;
    assert_eq!(a, 7, "先に走査した device は id を据え置く");
    assert_ne!(b, a, "重複した 2 つ目は再採番される");
    assert_ne!(m, a, "master_fx_chain の重複も再採番される");
    assert_ne!(m, b);
    assert!(b != 0 && m != 0, "再採番は sentinel を作らない");
    assert!(
        song.ids.next_device_id > a.max(b).max(m),
        "allocator は採番済み id より先へ進む"
    );

    // idempotent: 2 回目は何も動かない。
    let before = (a, b, m);
    song.ensure_ids();
    assert_eq!(
        (
            song.tracks[0].devices[0].id,
            song.tracks[1].devices[0].id,
            song.master_fx_chain[0].id
        ),
        before
    );
}

#[test]
fn project_file_roundtrip() {
    let pf = ProjectFile {
        version: CURRENT_VERSION,
        song: Song::default(),
        view: None,
    };
    assert_eq!(json_roundtrip(&pf), pf);
}

#[test]
fn empty_note_serializes_as_minimal_object() {
    // velocity 0 / pitch 0 / start 0 / duration 0 — lyric None is
    // skipped via `skip_serializing_if`, the rest are required fields.
    assert_eq!(
        serde_json::to_string(&Note::default()).unwrap(),
        r#"{"start_beat":0.0,"duration_beats":0.0,"pitch":0,"velocity":0}"#
    );
}

#[test]
fn note_with_lyric_serializes_compactly() {
    let note = Note {
        id: 0,
        start_beat: 0.5,
        duration_beats: 1.0,
        pitch: 60,
        velocity: 100,
        lyric: Some("こ".into()),
        muted: false,
    };
    assert_eq!(
        serde_json::to_string(&note).unwrap(),
        r#"{"start_beat":0.5,"duration_beats":1.0,"pitch":60,"velocity":100,"lyric":"こ"}"#
    );
    assert_eq!(json_roundtrip(&note), note);
}

#[test]
fn vocal_clip_roundtrip() {
    // §10 以降 notes は `clip_contents` (Midi)、per-clip 名は `clip_content_names` が SSoT。
    let mut song = Song {
        tracks: vec![Track {
            name: "Vocal".into(),
            source: InstrumentSource::Vocal,
            clips: vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 16.0,
                content_id: 1,
                speaker_id: 3061,
                singer_name: "中国うさぎ".into(),
                style_name: "ノーマル".into(),
                ..Default::default()
            }],
            ..Track::default()
        }],
        ..Song::default()
    };
    song.clip_contents.insert(
        1,
        ClipContent::Midi(MidiContent {
            notes: vec![
                Note {
                    id: 0,
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: Some("こ".into()),
                    muted: false,
                },
                Note {
                    id: 0,
                    start_beat: 1.5,
                    duration_beats: 0.5,
                    pitch: 62,
                    velocity: 100,
                    lyric: Some("ん".into()),
                    muted: false,
                },
            ],
            ..Default::default()
        }),
    );
    song.clip_content_names.insert(1, "こんにちは".into());
    song.ids.next_content_id = 2;
    assert_eq!(json_roundtrip(&song), song);
}

#[test]
fn current_version_is_pinned() {
    // Bumped to 23 for the single linear device chain: `Track`'s three
    // role-keyed chains (`instrument` / `midi_fx_chain` / `fx_chain`)
    // collapse into one `devices: Vec<PluginInstance>`, each carrying a
    // `ports: PortConfig` (roles are derived from position, not stored),
    // and `AutomationTarget::PluginParam { slot } → { device_index }`.
    // v22 files forward-migrate: the old fields deserialize into
    // private legacy slots and `Track::flatten_legacy_devices` (run from
    // `ensure_ids`) flattens them into `devices` and remaps lane slots.
    // Pinning the constant catches accidental rollback. See
    // `docs/plan_linear_chain.md`. v24 adds `Song.project_id`
    // (`docs/plan_fixme_33_clipboard.md`, clipboard same-project detection).
    // v25: 旧 `group_transform` 持ちトラックに `builtin.video.transform`
    // 配置 device を `ensure_ids` で補う (additive、値 migration 無し)。
    // v26 (`docs/plan_voicevox_talk.md`): `Clip.talk` 追加 + テキストオーバーレイ表示が
    // `builtin.video.subtitle` device gate になり、v25 以前は load 時に Text 持ち
    // トラックへ字幕デバイスを auto-insert (`project::migrate_text_overlay_to_subtitle_device`)。
    // v27: `Clip.muted` / `Note.muted` 追加 (clip / note mute の SSoT)、v26 以前の
    // per-event mute は `project::migrate_per_event_mute_to_clip_mute` で `Clip.muted` へ畳み込む。
    // v28: `ProjectFile.view: Option<ViewState>` 追加 (= ズーム/スクロール等の
    // 表示状態を Song の兄弟として同梱)。Song / IPC は無改変、旧ファイルは `#[serde(default)]`
    // で `view == None` に forward-migrate (migration 関数不要)。
    // v29 (`docs/plan_arch_refactor.md` §1): 安定 id addressing —
    // `PluginInstance.id: u64` / `Send.id` / note・audio event・automation
    // point の要素 id を追加し、`PluginParam.device_index` → `device_id`、
    // `SendGain.send_idx` → `send_id` へ移行。旧 positional 値は
    // deserialize 専用 legacy field 経由で `ensure_ids` が id へ写像する。
    // v30 (§10): ClipContent を untagged → tagged (`type` field) 化。
    // v31: `Song.loop_start_beat` / `loop_end_beat` を撤去し、再生ループ (ON/OFF +
    // 範囲) を session state + `ViewState.loop_region` へ移した (「聴き方の都合」 は
    // dirty を立てないが保存される)。v30 以前のファイルは
    // `project::legacy_song_loop_region` が Song 直下から拾って移行する。
    // v32 (r.md #44 / `docs/plan_clip_content_window.md`): `Clip` / `AutomationClip` に
    // `content_offset_beats` を追加 (= clip は共有 content への「窓」)。端 trim が
    // content を書き換えなくなり、linked clip の開始・終了が独立する。v31 以前は
    // `serde(default)` の 0.0 で読める (migration 関数不要)。
    // v33 (r.md #50 の follow-up): master の出力音量を `Song.master_gain` として
    // 保存する。従来は GUI のセッション状態にしか無く、保存しても開き直すと 0dB に
    // 戻っていた。v32 以前は `serde(default)` の 1.0 (unity) で読める。
    // v34 (r.md #71 プラグインのコピー / 移動): `BindingTarget::PluginParam.track` を
    // deserialize 専用 (`legacy_track`) に落とした。device を別トラックへ移せる
    // ようになったので、所属 track を保存すると stale になる (実行時の解決は
    // `device_id` の逆引き 1 本)。v33 以前は `track` を読んで
    // `legacy_device_index` の解決にだけ使う。
    assert_eq!(CURRENT_VERSION, 34);
}

#[test]
fn v25_ensure_ids_adds_transform_device_for_group_transform_tracks() {
    // 旧 group_transform 持ちトラックは ensure_ids で Transform
    // 配置 device を 1 つ得る (idempotent: 2 回呼んでも 1 つ)。group_transform 無しは付かない。
    let mut song = Song {
        tracks: vec![
            Track {
                id: 1,
                group_transform: Some(GroupTransform::default()),
                ..Track::default()
            },
            Track {
                id: 2,
                ..Track::default()
            },
        ],
        ids: IdAllocators {
            next_track_id: 3,
            ..Song::default().ids
        },
        ..Song::default()
    };
    song.ensure_ids();
    let t1 = song.track_by_id(1).unwrap();
    assert_eq!(
        t1.devices
            .iter()
            .filter(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID)
            .count(),
        1,
        "group_transform 持ちトラックに Transform device が 1 つ付くべき"
    );
    let t2 = song.track_by_id(2).unwrap();
    assert!(
        !t2.devices
            .iter()
            .any(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID),
        "group_transform 無しトラックには Transform device を付けない"
    );
    // idempotent: 再実行で増えない。
    song.ensure_ids();
    assert_eq!(
        song.track_by_id(1)
            .unwrap()
            .devices
            .iter()
            .filter(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID)
            .count(),
        1,
        "ensure_ids は idempotent (Transform device は重複しない)"
    );
}

#[test]
fn shared_clip_name_rename_is_group_wide_and_gc() {
    // 2 linked clips (同 content_id)。per-clip 名の SSoT は `clip_content_names` (§10 以降)。
    // 旧 v19 の per-clip `Clip.name` → 共有名 map ドレインは前処理層 (project.rs) のテストで担保。
    let mut song = Song {
        tracks: vec![Track {
            id: 1,
            clips: vec![
                Clip {
                    id: 1,
                    length_beats: 4.0,
                    content_id: 7,
                    ..Clip::default()
                },
                Clip {
                    id: 2,
                    start_beat: 4.0,
                    length_beats: 4.0,
                    content_id: 7,
                    ..Clip::default()
                },
            ],
            ..Track::default()
        }],
        ..Song::default()
    };
    song.ensure_clip_contents();
    song.set_content_name(7, "Verse".into());
    assert_eq!(song.content_name(7), "Verse");

    // Renaming via the shared map renames the whole linked group: both
    // clips resolve the same name through their shared content_id.
    song.set_content_name(7, "Chorus".into());
    let cid0 = song.tracks[0].clips[0].content_id;
    let cid1 = song.tracks[0].clips[1].content_id;
    assert_eq!(song.content_name(cid0), "Chorus");
    assert_eq!(song.content_name(cid1), "Chorus");

    // fork_content copies the name under a fresh id, then diverges
    // independently of the source group.
    let forked = song.fork_content(7);
    assert_ne!(forked, 7);
    assert_eq!(song.content_name(forked), "Chorus");
    song.set_content_name(forked, "Bridge".into());
    assert_eq!(song.content_name(forked), "Bridge");
    assert_eq!(song.content_name(7), "Chorus");

    // GC drops names whose content_id is no longer referenced by any
    // clip (the fork has no clip pointing at it).
    song.gc_clip_contents();
    assert_eq!(song.content_name(7), "Chorus");
    assert!(!song.clip_content_names.contains_key(&forked));
}

#[test]
fn v17_track_and_clip_load_forward_with_none_color() {
    // A v17 .daw file (no `color` key on Track / Clip) must load with
    // `color == None` (= derived palette / inherit), proving the v18
    // field is `#[serde(default)]`.
    let v17_json = r#"{
            "id": 3,
            "name": "Lead",
            "volume": 1.0,
            "pan": 0.0,
            "next_clip_id": 2,
            "clips": [
                {
                    "id": 1,
                    "name": "C",
                    "start_beat": 0.0,
                    "length_beats": 4.0,
                    "content_id": 1
                }
            ]
        }"#;
    let track: Track = serde_json::from_str(v17_json).unwrap();
    assert_eq!(track.color, None);
    assert_eq!(track.clips[0].color, None);
}

#[test]
fn track_and_clip_color_bincode_round_trip() {
    // v18 color fields survive a bincode encode/decode (the IPC + on-disk
    // path). `None` and `Some` both round-trip.
    let cfg = bincode::config::standard();
    let track = Track {
        id: 9,
        color: Some([0.25, 0.5, 0.75]),
        clips: vec![
            Clip {
                id: 1,
                color: None,
                ..Clip::default()
            },
            Clip {
                id: 2,
                color: Some([0.1, 0.2, 0.3]),
                ..Clip::default()
            },
        ],
        ..Track::default()
    };
    let bytes = bincode::encode_to_vec(&track, cfg).unwrap();
    let (decoded, _): (Track, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
    assert_eq!(decoded.color, Some([0.25, 0.5, 0.75]));
    assert_eq!(decoded.clips[0].color, None);
    assert_eq!(decoded.clips[1].color, Some([0.1, 0.2, 0.3]));
}

#[test]
fn v4_track_loads_forward_with_default_routing_fields() {
    // A v4 .daw file (no `parent_group_id` key) must round-trip through
    // serde_json into a v5 `Track` with defaulted graph fields.
    let v4_json = r#"{
            "id": 7,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
    let track: Track = serde_json::from_str(v4_json).unwrap();
    assert_eq!(track.id, 7);
    assert_eq!(track.parent_group_id, None);
}

#[test]
fn track_with_parent_group_id_roundtrip() {
    // The "group" role is implicit (track 1 here ends up acting as
    // a group because track 2 points at it via parent_group_id).
    // No explicit `kind` field exists.
    let song = Song {
        tracks: vec![
            Track {
                id: 1,
                name: "Drums".into(),
                parent_group_id: None,
                ..Track::default()
            },
            Track {
                id: 2,
                name: "Kick".into(),
                parent_group_id: Some(1),
                ..Track::default()
            },
        ],
        ..Song::default()
    };
    let restored: Song = json_roundtrip(&song);
    assert_eq!(restored, song);
}

// ====================================================================
// Aux send / return (v17) — `Track.sends: Vec<Send>`
// ====================================================================

#[test]
fn v16_track_loads_with_empty_sends() {
    // A v16 .daw file has no `sends` key; forward-migration via
    // `#[serde(default)]` must populate an empty Vec.
    let v16_json = r#"{
            "id": 5,
            "name": "Vocal",
            "volume": 1.0,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
    let track: Track = serde_json::from_str(v16_json).unwrap();
    assert_eq!(track.id, 5);
    assert!(track.sends.is_empty());
}

#[test]
fn track_with_sends_roundtrips_through_serde_and_bincode() {
    // Vocal sends post-fader to a Reverb return and pre-fader (muted)
    // to a Delay return. Both serde (save) and bincode (IPC) must
    // preserve the sends exactly.
    let song = Song {
        tracks: vec![
            Track {
                id: 1,
                name: "Vocal".into(),
                sends: vec![
                    Send {
                        id: 0,
                        dest_track_id: 2,
                        gain: 0.5,
                        mode: SendMode::PostFader,
                        enabled: true,
                    },
                    Send {
                        id: 0,
                        dest_track_id: 3,
                        gain: 1.0,
                        mode: SendMode::PreFader,
                        enabled: false,
                    },
                ],
                ..Track::default()
            },
            Track {
                id: 2,
                name: "Reverb".into(),
                ..Track::default()
            },
            Track {
                id: 3,
                name: "Delay".into(),
                ..Track::default()
            },
        ],
        ..Song::default()
    };

    assert_eq!(
        json_roundtrip(&song),
        song,
        "serde (save) must preserve sends"
    );

    let cfg = bincode::config::standard();
    let bytes = bincode::encode_to_vec(&song, cfg).unwrap();
    let (decoded, _): (Song, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
    assert_eq!(decoded, song, "bincode (IPC) must preserve sends");
}

#[test]
fn ensure_ids_remaps_send_dest_track_id() {
    // A Vocal track sends to a Reverb return whose id is the `0`
    // sentinel. After ensure_ids rebases the return, the send's
    // `dest_track_id` must follow — otherwise the send dangles and
    // `compile_schedule` silently drops it (no reverb).
    let mut song = Song {
        tracks: vec![
            Track {
                id: 0, // sentinel — Reverb return, rebased by ensure_ids
                name: "Reverb".into(),
                ..Track::default()
            },
            Track {
                id: 1,
                name: "Vocal".into(),
                sends: vec![Send {
                    id: 0,
                    dest_track_id: 0, // points at Reverb's sentinel id
                    gain: 0.5,
                    mode: SendMode::PostFader,
                    enabled: true,
                }],
                ..Track::default()
            },
        ],
        ids: IdAllocators {
            next_track_id: 2,
            ..Song::default().ids
        },
        ..Song::default()
    };

    song.ensure_ids();

    let new_reverb_id = song.tracks[0].id;
    assert_ne!(new_reverb_id, 0, "ensure_ids should replace sentinel id 0");
    assert_eq!(
        song.tracks[1].sends[0].dest_track_id, new_reverb_id,
        "send dest pointing at the sentinel must be remapped to the new id"
    );
}

// ====================================================================
// Automation (v8) — `Track.automation_lanes` + `ClipContent::Automation`
// ====================================================================

#[test]
fn v7_track_loads_with_empty_automation_lanes() {
    // A v7 .daw file has no `automation_lanes` / `next_lane_id` keys.
    // Forward-migration via `#[serde(default)]` must populate empty
    // Vec / 0 without losing other fields.
    let v7_json = r#"{
            "id": 3,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
    let track: Track = serde_json::from_str(v7_json).unwrap();
    assert_eq!(track.id, 3);
    assert!(track.automation_lanes.is_empty());
    assert_eq!(track.next_lane_id, 0);
}

#[test]
fn ensure_lane_ids_assigns_sentinel() {
    // Lane id 0 (sentinel) gets a fresh id; non-zero lane ids are
    // left alone but bump `next_lane_id` above the highest seen.
    let mut track = Track {
        id: 1,
        name: "T".into(),
        automation_lanes: vec![
            AutomationLane {
                id: 0,
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    1.0,
                )
            },
            AutomationLane {
                id: 5,
                ..AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan), 0.0)
            },
        ],
        next_lane_id: 0,
        ..Track::default()
    };
    track.ensure_lane_ids();
    // Sentinel got reassigned; counter is bumped above max seen.
    assert_ne!(track.automation_lanes[0].id, 0);
    assert_eq!(track.automation_lanes[1].id, 5);
    assert!(track.next_lane_id > 5);
}

#[test]
fn automation_clip_content_roundtrip() {
    // A song with one automation lane + one clip + one point
    // round-trips through serde_json bit-for-bit. Exercises
    // `ClipContent::Automation` untagged dispatch.
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Automation(AutomationContent {
            next_point_id: 0,
            points: vec![
                AutomationPoint {
                    id: 0,
                    time_beat: 0.0,
                    value: 0.5,
                    curve: AutomationCurve::Linear,
                },
                AutomationPoint {
                    id: 0,
                    time_beat: 4.0,
                    value: 1.0,
                    curve: AutomationCurve::Bezier { tension: 0.25 },
                },
            ],
        }),
    );
    song.tracks.push(Track {
        id: 1,
        name: "T".into(),
        automation_lanes: vec![AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "auto1".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.85,
            )
        }],
        next_lane_id: 2,
        ..Track::default()
    });

    let restored: Song = json_roundtrip(&song);
    assert_eq!(restored, song);
    assert!(matches!(
        restored.clip_contents[&cid],
        ClipContent::Automation(_)
    ));
}

#[test]
fn automation_clip_counts_toward_clip_content_refcount() {
    // Same `content_id` shared by a MIDI clip *and* an automation
    // clip should refcount as 2 — `clip_content_refcount` walks
    // both `Track.clips` and `automation_lanes[].clips`.
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents
        .insert(cid, ClipContent::Automation(AutomationContent::default()));
    song.tracks.push(Track {
        id: 1,
        name: "T".into(),
        clips: vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        }],
        automation_lanes: vec![AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "auto1".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                1.0,
            )
        }],
        ..Track::default()
    });
    assert_eq!(song.clip_content_refcount(cid), 2);
}

#[test]
fn gc_clip_contents_keeps_automation_clip_references() {
    // A content_id only referenced by an automation clip must
    // survive `gc_clip_contents` — earlier impl walked only
    // `Track.clips` and would drop it.
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents
        .insert(cid, ClipContent::Automation(AutomationContent::default()));
    song.tracks.push(Track {
        id: 1,
        name: "T".into(),
        automation_lanes: vec![AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "auto1".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                1.0,
            )
        }],
        ..Track::default()
    });
    song.gc_clip_contents();
    assert!(song.clip_contents.contains_key(&cid));
}

/// v29: `Song::remove_track_send` は安定 send id で削除し、 その send を
/// 狙う SendGain lane だけを除去する。 残る send への参照は id なので
/// **無変更のまま正しい** (positional reindex 儀式は廃止)。
#[test]
fn remove_track_send_drops_only_matching_send_gain_lanes() {
    let send_lane = |sid: u32| {
        AutomationLane::new(
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_id: sid,
                legacy_send_idx: None,
            }),
            1.0,
        )
    };
    let mk_send = |sid: u32, dest: u32| crate::model::Send {
        id: sid,
        dest_track_id: dest,
        gain: 1.0,
        mode: crate::model::SendMode::PostFader,
        enabled: true,
    };
    let mut song = Song::default();
    song.tracks.push(Track {
        id: 42,
        sends: vec![mk_send(10, 1), mk_send(11, 2), mk_send(12, 3)],
        next_send_id: 13,
        automation_lanes: vec![send_lane(10), send_lane(11), send_lane(12)],
        ..Track::default()
    });
    // send id 11 (dest 2) を削除。
    assert!(song.remove_track_send(42, 11));
    let t = song.track_by_id(42).unwrap();
    assert_eq!(t.sends.len(), 2);
    assert_eq!(t.sends[0].dest_track_id, 1);
    assert_eq!(t.sends[1].dest_track_id, 3);
    let ids: Vec<u32> = t
        .automation_lanes
        .iter()
        .filter_map(|l| match l.target {
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, .. }) => {
                Some(send_id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![10, 12], "id 11 の lane だけ除去、残りは無変更");
    // 不在 id / 不在 track は false (no-op)。
    assert!(!song.remove_track_send(42, 99));
    assert!(!song.remove_track_send(999, 10));
}

/// v23-28 の PluginParam MIDI binding は `device_index` を持ち legacy_device_index に載る
/// (`ensure_ids` が device_id へ写像)。v29 の device_id はそのまま (bincode 往復も一致)。
/// 旧 v22 `slot` は load 時 JSON 前処理 (`project::migrate_legacy_device_chains`) が
/// device_index へ解決するので in-memory 型には現れない (project.rs のテストで担保)。
#[test]
fn binding_target_plugin_param_legacy_compat() {
    // v23-28 JSON (device_index) は legacy_device_index に載る。
    let json_v28 = r#"{"PluginParam":{"track":1,"device_index":2,"param_id":5}}"#;
    let bt2: BindingTarget = serde_json::from_str(json_v28).unwrap();
    assert!(matches!(
        bt2,
        BindingTarget::PluginParam {
            device_id: 0,
            legacy_device_index: Some(2),
            legacy_track: Some(1),
            ..
        }
    ));
    // v29 JSON (device_id) はそのまま。 bincode 往復 (IPC 経路) も一致。
    let bt3 = BindingTarget::PluginParam {
        device_id: 42,
        param_id: 5,
        legacy_device_index: None,
        legacy_track: None,
    };
    let via_json = json_roundtrip(&bt3);
    assert_eq!(bt3, via_json);
    let cfg = bincode::config::standard();
    let bytes = bincode::encode_to_vec(bt3, cfg).unwrap();
    let (back, _): (BindingTarget, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
    assert_eq!(bt3, back);
}

#[test]
fn gc_clip_contents_keeps_song_lane_references() {
    // Regression: a content_id only referenced by a song-level
    // automation clip (SongTempo master lane) must survive
    // `gc_clip_contents`. The earlier impl walked only `tracks[]` and
    // dropped it, silently deleting tempo-automation curves on save.
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents
        .insert(cid, ClipContent::Automation(AutomationContent::default()));
    song.song_lanes.push(AutomationLane {
        id: 1,
        clips: vec![AutomationClip {
            id: 1,
            name: "tempo".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            content_offset_beats: 0.0,
        }],
        ..AutomationLane::new(AutomationTarget::SongTempo, 120.0)
    });
    song.gc_clip_contents();
    assert!(
        song.clip_contents.contains_key(&cid),
        "song-lane automation content must survive GC"
    );
    assert_eq!(
        song.clip_content_refcount(cid),
        1,
        "song-lane clip must count toward refcount"
    );
}

#[test]
fn ensure_clip_contents_reassigns_song_lane_sentinel_ids() {
    // Regression: a song-lane clip carrying the sentinel content_id 0
    // must get a fresh id and a content entry, else automation eval /
    // GUI lookup always fall back to empty.
    let mut song = Song::default();
    song.song_lanes.push(AutomationLane {
        id: 1,
        clips: vec![AutomationClip {
            id: 1,
            name: "tempo".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: 0,
            content_offset_beats: 0.0,
        }],
        ..AutomationLane::new(AutomationTarget::SongTempo, 120.0)
    });
    song.ensure_clip_contents();
    let cid = song.song_lanes[0].clips[0].content_id;
    assert_ne!(cid, 0, "sentinel content_id must be reassigned");
    assert!(
        song.clip_contents.contains_key(&cid),
        "reassigned song-lane content must have an entry"
    );
}

#[test]
fn automation_target_hashes_distinguish_variants() {
    // Targets are used as HashMap keys (e.g. last-touched param
    // bookkeeping). Same-shape variants with different payloads
    // must produce different hashes.
    use std::collections::HashSet;
    let mut s = HashSet::new();
    s.insert(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume));
    s.insert(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan));
    s.insert(AutomationTarget::PluginParam {
        device_id: 0,
        param_id: 7,
        legacy_device_index: None,
    });
    s.insert(AutomationTarget::PluginParam {
        device_id: 1,
        param_id: 7,
        legacy_device_index: None,
    });
    assert_eq!(s.len(), 4);
}

// ====================================================================
// Video (v12) — `Track.kind`, `Song.video_sources`,
// `ClipContent::Video`, project-level resolution / framerate.
// See `docs/plan_video.md`.
// ====================================================================

#[test]
fn v11_track_loads_forward_with_default_kind() {
    // A v11 `.daw` file has no `kind` key on `Track`. Forward-
    // migration via `#[serde(default)]` must populate `Audio`.
    let v11_json = r#"{
            "id": 4,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
    let track: Track = serde_json::from_str(v11_json).unwrap();
    assert_eq!(track.id, 4);
}

#[test]
fn v11_song_loads_forward_with_default_video_fields() {
    // A v11 `.daw` file has no `video_sources` / `next_video_source_id`
    // / `video_resolution` / `video_framerate` keys. Forward-migration
    // via `#[serde(default)]` must populate empty / 1080p / 30fps.
    let v11_json = r#"{
            "bpm": 120.0,
            "time_sig": [4, 4],
            "length_beats": 64.0
        }"#;
    let song: Song = serde_json::from_str(v11_json).unwrap();
    assert!(song.media.video_sources.is_empty());
    assert_eq!(song.ids.next_video_source_id, 0);
    assert_eq!(song.video_resolution, (1920, 1080));
    assert!((song.video_framerate - 30.0).abs() < f32::EPSILON);
}

#[test]
fn video_track_with_clip_content_roundtrip() {
    // A song with one video track + one video clip + one event
    // round-trips through serde_json bit-for-bit. Exercises
    // `ClipContent::Video` untagged dispatch against the existing
    // `Midi` / `Audio` / `Automation` variants.
    let mut song = Song::default();
    let vsrc_id = song.alloc_video_source_id();
    song.media.video_sources.insert(
        vsrc_id,
        VideoSource {
            path: VideoSourcePath::ProjectRelative("samples/clip.mp4".into()),
            width: 1920,
            height: 1080,
            framerate: 30.0,
            duration_micros: 10_000_000,
            codec: "h264".into(),
            audio_source_id: None,
        },
    );
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Video(VideoContent {
            events: vec![VideoEvent {
                source_id: vsrc_id,
                event_start_in_clip_beats: 0.0,
                event_length_beats: 4.0,
                source_start_micros: 0,
                source_end_micros: 2_000_000,
                muted: false,
                fade_in_beats: 0.25,
                fade_out_beats: 0.5,
                fade_in_curve: FadeCurve::Linear,
                fade_out_curve: FadeCurve::SCurve,
            }],
        }),
    );
    song.tracks.push(Track {
        id: 1,
        name: "Vid".into(),
        clips: vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        }],
        next_clip_id: 2,
        ..Track::default()
    });

    let restored: Song = json_roundtrip(&song);
    assert_eq!(restored, song);
    assert!(matches!(
        restored.clip_contents[&cid],
        ClipContent::Video(_)
    ));
}

#[test]
fn tagged_clip_content_round_trips_audio_and_video() {
    // (v30 §10) tagged `ClipContent`: `"type"` タグで Audio / Video を明示判別する
    // (旧 `#[serde(untagged)]` + events の required-field 依存は撤去済)。
    let audio_json = r#"{
            "type": "Audio",
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 1.0,
                "source_start_frames": 0,
                "source_end_frames": 44100,
                "gain_db": 0.0,
                "pan": 0.0,
                "pitch_semitones": 0.0,
                "formant_semitones": 0.0,
                "stretch_mode": "Raw",
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear",
                "reversed": false,
                "muted": false
            }]
        }"#;
    let video_json = r#"{
            "type": "Video",
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 1.0,
                "source_start_micros": 0,
                "source_end_micros": 1000000,
                "muted": false,
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear"
            }]
        }"#;
    let audio: ClipContent = serde_json::from_str(audio_json).unwrap();
    let video: ClipContent = serde_json::from_str(video_json).unwrap();
    assert!(matches!(audio, ClipContent::Audio(_)));
    assert!(matches!(video, ClipContent::Video(_)));
}

// ---- rescale_raw_clips_for_bpm (r.md #7) ----

fn rescale_event(mode: StretchMode, start: f64, len: f64) -> AudioEvent {
    AudioEvent {
        source_id: 1,
        event_start_in_clip_beats: start,
        event_length_beats: len,
        source_start_frames: 0,
        source_end_frames: 48_000,
        stretch_mode: mode,
        fade_in_beats: 0.5,
        fade_out_beats: 0.25,
        ..Default::default()
    }
}

/// 1 track / 1 clip の Song を組み立て、(song, content_id) を返す。clip は
/// `start_beat` / `length_beats` に置かれ、与えた `events` の content を参照する。
fn rescale_song(events: Vec<AudioEvent>, start_beat: f64, length_beats: f64) -> (Song, ContentId) {
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Audio(AudioContent {
            events,
            next_event_id: 0,
        }),
    );
    song.tracks = vec![Track {
        id: 1,
        clips: vec![Clip {
            id: 1,
            start_beat,
            length_beats,
            content_id: cid,
            ..Default::default()
        }],
        next_clip_id: 2,
        ..Track::default()
    }];
    (song, cid)
}

#[test]
fn rescale_raw_doubles_length_on_bpm_double() {
    // Raw clip を BPM 120 → 240 (ratio 2.0)。event / clip の拍量は秒固定で 2 倍、
    // clip.start_beat は拍固定。
    let (mut song, cid) = rescale_song(vec![rescale_event(StretchMode::Raw, 1.0, 4.0)], 8.0, 4.0);
    song.rescale_raw_clips_for_bpm(120.0, 240.0);
    let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
        panic!("audio");
    };
    let ev = &a.events[0];
    assert_eq!(ev.event_start_in_clip_beats, 2.0);
    assert_eq!(ev.event_length_beats, 8.0);
    assert_eq!(ev.fade_in_beats, 1.0);
    assert_eq!(ev.fade_out_beats, 0.5);
    let clip = &song.tracks[0].clips[0];
    assert_eq!(clip.start_beat, 8.0, "start は拍固定");
    assert_eq!(clip.length_beats, 8.0);
}

#[test]
fn rescale_raw_halves_length_on_bpm_half() {
    let (mut song, cid) = rescale_song(vec![rescale_event(StretchMode::Raw, 2.0, 4.0)], 0.0, 4.0);
    song.rescale_raw_clips_for_bpm(120.0, 60.0);
    let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
        panic!("audio");
    };
    assert_eq!(a.events[0].event_length_beats, 2.0);
    assert_eq!(a.events[0].event_start_in_clip_beats, 1.0);
    assert_eq!(song.tracks[0].clips[0].length_beats, 2.0);
}

#[test]
fn rescale_leaves_non_raw_modes_untouched() {
    // Stretch / Repitch / Slice は拍固定。一切変えない。
    for mode in [
        StretchMode::Stretch,
        StretchMode::Repitch,
        StretchMode::Slice,
    ] {
        let (mut song, cid) = rescale_song(vec![rescale_event(mode, 1.0, 4.0)], 0.0, 4.0);
        song.rescale_raw_clips_for_bpm(120.0, 240.0);
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        assert_eq!(a.events[0].event_length_beats, 4.0, "{mode:?} 不変");
        assert_eq!(a.events[0].event_start_in_clip_beats, 1.0, "{mode:?} 不変");
        assert_eq!(song.tracks[0].clips[0].length_beats, 4.0, "{mode:?} 不変");
    }
}

#[test]
fn rescale_skips_mixed_mode_content() {
    // Raw と Stretch が混在する content は「全 event が Raw」でないので
    // content / clip ともに据え置き (event 単位でなく clip 単位の判定)。
    let (mut song, cid) = rescale_song(
        vec![
            rescale_event(StretchMode::Raw, 0.0, 2.0),
            rescale_event(StretchMode::Stretch, 2.0, 2.0),
        ],
        0.0,
        4.0,
    );
    song.rescale_raw_clips_for_bpm(120.0, 240.0);
    let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
        panic!("audio");
    };
    assert_eq!(a.events[0].event_length_beats, 2.0);
    assert_eq!(a.events[1].event_length_beats, 2.0);
    assert_eq!(song.tracks[0].clips[0].length_beats, 4.0);
}

#[test]
fn rescale_shared_content_scales_once_all_clips() {
    // 同一 Raw content を 2 clip が共有。content events は一度だけスケールされ、
    // 参照する両 clip の length がスケールされる (start は各々固定)。
    let mut song = Song::default();
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Audio(AudioContent {
            events: vec![rescale_event(StretchMode::Raw, 0.0, 4.0)],
            next_event_id: 0,
        }),
    );
    song.tracks = vec![Track {
        id: 1,
        clips: vec![
            Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                ..Default::default()
            },
            Clip {
                id: 2,
                start_beat: 16.0,
                length_beats: 4.0,
                content_id: cid,
                ..Default::default()
            },
        ],
        next_clip_id: 3,
        ..Track::default()
    }];
    song.rescale_raw_clips_for_bpm(100.0, 150.0); // ratio 1.5
    let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
        panic!("audio");
    };
    assert_eq!(a.events[0].event_length_beats, 6.0);
    assert_eq!(song.tracks[0].clips[0].length_beats, 6.0);
    assert_eq!(song.tracks[0].clips[1].length_beats, 6.0);
    assert_eq!(song.tracks[0].clips[1].start_beat, 16.0, "start は拍固定");
}

#[test]
fn rescale_degenerate_inputs_are_noop() {
    for (old, new) in [
        (0.0f32, 240.0f32),
        (120.0, 0.0),
        (f32::NAN, 240.0),
        (120.0, 120.0),
    ] {
        let (mut song, cid) =
            rescale_song(vec![rescale_event(StretchMode::Raw, 1.0, 4.0)], 0.0, 4.0);
        song.rescale_raw_clips_for_bpm(old, new);
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        assert_eq!(a.events[0].event_length_beats, 4.0, "old={old} new={new}");
        assert_eq!(
            a.events[0].event_start_in_clip_beats, 1.0,
            "old={old} new={new}"
        );
        assert_eq!(
            song.tracks[0].clips[0].length_beats, 4.0,
            "old={old} new={new}"
        );
    }
}

#[test]
fn alloc_video_source_id_bumps_counter() {
    let mut song = Song::default();
    let a = song.alloc_video_source_id();
    let b = song.alloc_video_source_id();
    assert_eq!(a, 1);
    assert_eq!(b, 2);
    assert_eq!(song.ids.next_video_source_id, 3);
}

#[test]
fn video_source_refcount_counts_events() {
    let mut song = Song::default();
    let vid = song.alloc_video_source_id();
    song.media.video_sources.insert(
        vid,
        VideoSource {
            path: VideoSourcePath::Absolute("/tmp/v.mp4".into()),
            width: 640,
            height: 480,
            framerate: 30.0,
            duration_micros: 1_000_000,
            codec: "h264".into(),
            audio_source_id: None,
        },
    );
    let cid_a = song.alloc_content_id();
    song.clip_contents.insert(
        cid_a,
        ClipContent::Video(VideoContent {
            events: vec![
                VideoEvent {
                    source_id: vid,
                    ..VideoEvent::default()
                },
                VideoEvent {
                    source_id: vid,
                    ..VideoEvent::default()
                },
            ],
        }),
    );
    let cid_b = song.alloc_content_id();
    song.clip_contents.insert(
        cid_b,
        ClipContent::Video(VideoContent {
            events: vec![VideoEvent {
                source_id: vid,
                ..VideoEvent::default()
            }],
        }),
    );
    assert_eq!(song.video_source_refcount(vid), 3);
}

#[test]
fn gc_video_sources_drops_orphans() {
    let mut song = Song::default();
    let live_id = song.alloc_video_source_id();
    let orphan_id = song.alloc_video_source_id();
    song.media.video_sources.insert(
        live_id,
        VideoSource {
            path: VideoSourcePath::Absolute("/tmp/live.mp4".into()),
            width: 640,
            height: 480,
            framerate: 30.0,
            duration_micros: 1_000_000,
            codec: "h264".into(),
            audio_source_id: None,
        },
    );
    song.media.video_sources.insert(
        orphan_id,
        VideoSource {
            path: VideoSourcePath::Absolute("/tmp/orphan.mp4".into()),
            width: 640,
            height: 480,
            framerate: 30.0,
            duration_micros: 1_000_000,
            codec: "h264".into(),
            audio_source_id: None,
        },
    );
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Video(VideoContent {
            events: vec![VideoEvent {
                source_id: live_id,
                ..VideoEvent::default()
            }],
        }),
    );

    song.gc_video_sources();
    assert!(song.media.video_sources.contains_key(&live_id));
    assert!(!song.media.video_sources.contains_key(&orphan_id));
}

// =========================================================================
// Image overlay (v13, docs/plan_image_overlay.md §P1 invariants)
// =========================================================================

#[test]
fn tagged_clip_content_round_trips_image() {
    // (v30 §10) tagged `ClipContent`: `"type": "Image"` で Image variant を明示判別する
    // (旧 untagged の opacity 依存 dispatch は撤去済)。
    let image_json = r#"{
            "type": "Image",
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 4.0,
                "x": 0.1,
                "y": 0.1,
                "w": 0.3,
                "h": 0.3,
                "opacity": 1.0,
                "muted": false,
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear"
            }]
        }"#;
    let image: ClipContent = serde_json::from_str(image_json).unwrap();
    assert!(matches!(image, ClipContent::Image(_)));
}

#[test]
fn image_source_without_name_field_deserializes_with_empty_name() {
    // v21 `.daw` files stored `ImageSource` without a `name` key. Loading
    // into v22 must succeed with `name` defaulting to "" (per
    // `#[serde(default)]`); the inspector then falls back to the on-disk
    // file name. A v22 source carries the original import name verbatim.
    let v21_json = r#"{
            "path": { "Absolute": "/img/_a1b2c3d4.png" },
            "width": 64,
            "height": 64,
            "format": "Png"
        }"#;
    let src: ImageSource = serde_json::from_str(v21_json).unwrap();
    assert_eq!(src.name, "");

    // Round-trip a v22 source with the original (Japanese) name.
    let named = ImageSource {
        path: ImageSourcePath::Absolute("/img/_a1b2c3d4.png".into()),
        name: "あ.png".into(),
        width: 64,
        height: 64,
        format: "Png".into(),
    };
    let json = serde_json::to_string(&named).unwrap();
    let back: ImageSource = serde_json::from_str(&json).unwrap();
    assert_eq!(back.name, "あ.png");
}

#[test]
fn alloc_image_source_id_bumps_counter() {
    let mut song = Song::default();
    let a = song.alloc_image_source_id();
    let b = song.alloc_image_source_id();
    assert_eq!(a, 1);
    assert_eq!(b, 2);
    assert_eq!(song.ids.next_image_source_id, 3);
}

#[test]
fn image_source_refcount_counts_events_across_clips() {
    let mut song = Song::default();
    let img = song.alloc_image_source_id();
    song.media.image_sources.insert(
        img,
        ImageSource {
            path: ImageSourcePath::Absolute("/tmp/logo.png".into()),
            name: "logo.png".into(),
            width: 256,
            height: 256,
            format: "Png".into(),
        },
    );
    let cid_a = song.alloc_content_id();
    song.clip_contents.insert(
        cid_a,
        ClipContent::Image(ImageContent {
            events: vec![
                ImageEvent {
                    source_id: img,
                    ..ImageEvent::default()
                },
                ImageEvent {
                    source_id: img,
                    ..ImageEvent::default()
                },
            ],
        }),
    );
    let cid_b = song.alloc_content_id();
    song.clip_contents.insert(
        cid_b,
        ClipContent::Image(ImageContent {
            events: vec![ImageEvent {
                source_id: img,
                ..ImageEvent::default()
            }],
        }),
    );
    assert_eq!(song.image_source_refcount(img), 3);
}

#[test]
fn gc_image_sources_drops_orphans() {
    let mut song = Song::default();
    let live_id = song.alloc_image_source_id();
    let orphan_id = song.alloc_image_source_id();
    song.media.image_sources.insert(
        live_id,
        ImageSource {
            path: ImageSourcePath::Absolute("/tmp/live.png".into()),
            name: "live.png".into(),
            width: 256,
            height: 256,
            format: "Png".into(),
        },
    );
    song.media.image_sources.insert(
        orphan_id,
        ImageSource {
            path: ImageSourcePath::Absolute("/tmp/orphan.png".into()),
            name: "orphan.png".into(),
            width: 256,
            height: 256,
            format: "Png".into(),
        },
    );
    let cid = song.alloc_content_id();
    song.clip_contents.insert(
        cid,
        ClipContent::Image(ImageContent {
            events: vec![ImageEvent {
                source_id: live_id,
                ..ImageEvent::default()
            }],
        }),
    );

    song.gc_image_sources();
    assert!(song.media.image_sources.contains_key(&live_id));
    assert!(!song.media.image_sources.contains_key(&orphan_id));
}

#[test]
fn v12_forward_migrates_image_fields_to_default() {
    // v12 file (= no image_sources / next_image_source_id keys)
    // must deserialize cleanly into v13 Song with default-empty
    // image pool and next_id == 0.
    let v12_song_json = serde_json::json!({
        "bpm": 120.0,
        "time_sig": [4, 4],
        "length_beats": 64.0,
    });
    let song: Song = serde_json::from_value(v12_song_json).unwrap();
    assert!(song.media.image_sources.is_empty());
    assert_eq!(song.ids.next_image_source_id, 0);
}

// ---- プラグイン報告 latency は保存対象外 (r.md #9) ---------------------------

/// 旧 `.daw` は plugin が報告した latency の合計を
/// `Track::reported_latency_samples` / `Song::master_reported_latency_samples`
/// として **保存していた**。 これは実行時の観測値であって曲の中身ではなく、
/// 開き直したときに host の報告と食い違って「開いただけで `*`」 になっていた
/// ので、モデルから外して engine へ device 単位で直送する形にした。
///
/// ここで守るのは移行の後方互換 — 旧キーが残った file はそのまま読め、
/// 保存し直すとキーが消える (= 派生値がファイルに焼き付かない)。
#[test]
fn legacy_reported_latency_keys_are_ignored_and_never_written_back() {
    let legacy = serde_json::json!({
        "bpm": 120.0,
        "time_sig": [4, 4],
        "length_beats": 64.0,
        "master_reported_latency_samples": 2048,
        "tracks": [{
            "id": 1,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1,
            "reported_latency_samples": 512,
        }],
    });
    let song: Song = serde_json::from_value(legacy).expect("旧キー付きでも読める");
    assert_eq!(song.tracks.len(), 1);

    let written = serde_json::to_value(&song).unwrap();
    assert!(
        written.get("master_reported_latency_samples").is_none(),
        "master の報告 latency は保存しない"
    );
    assert!(
        written["tracks"][0]
            .get("reported_latency_samples")
            .is_none(),
        "track の報告 latency は保存しない"
    );
}
