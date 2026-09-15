//! `window_edit`: クリップの窓に見えている片への編集は、**分割前に掛けてから分割したのと同じ**になり、
//! content を共有する反対側の窓の片には効かない。

use super::*;

const EPS: f64 = 1e-9;

/// 伸縮・移調・fade を持つ 4 拍の event (source 2 秒 @ 120 BPM を 4 拍に Repitch)。
fn take() -> AudioEvent {
    AudioEvent {
        id: 7,
        source_id: 1,
        event_length_beats: 4.0,
        source_start_frames: 12_000,
        source_end_frames: 108_000,
        stretch_mode: StretchMode::Repitch,
        pitch_semitones: 2.0,
        fade_in_beats: 0.5,
        fade_out_beats: 0.75,
        ..AudioEvent::default()
    }
}

const CUTS: [f64; 3] = [1.0, 2.25, 3.0];
/// 48 kHz @ 120 BPM の native rate。
const NATIVE_FPB: f64 = 24_000.0;

/// 位置・長さ・fade・take・写像が浮動小数の丸めを除いて同じか。
fn assert_same_pieces(label: &str, got: &[AudioEvent], want: &[AudioEvent]) {
    assert_eq!(got.len(), want.len(), "{label}: 片の数");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        let close = |a: f64, b: f64| (a - b).abs() <= EPS;
        let (gf, wf) = (g.fade(), w.fade());
        assert!(
            close(gf.start_in_clip_beats, wf.start_in_clip_beats)
                && close(gf.len_beats, wf.len_beats)
                && close(gf.fade_in_beats, wf.fade_in_beats)
                && close(gf.fade_out_beats, wf.fade_out_beats)
                && close(gf.fade_in_lead_beats, wf.fade_in_lead_beats)
                && close(gf.fade_out_trail_beats, wf.fade_out_trail_beats)
                && close(g.take_head_beats, w.take_head_beats)
                && close(g.take_tail_beats, w.take_tail_beats),
            "{label}: 片 {i} の窓 / fade / take が違う\n got  {g:?}\n want {w:?}"
        );
        assert_eq!(g.material(), w.material(), "{label}: 片 {i} の中身 (写像・値) が違う");
    }
}

/// `edit` を 1 つの event に掛けてから割った片と、割った片のひと続きに掛けた片が一致する。
fn assert_edit_commutes_with_split(label: &str, edit: impl Fn(&mut AudioEvent)) {
    let mut edited = take();
    edit(&mut edited);
    let want = split_pieces(&edited, CUTS, 0.0);

    let mut got = split_pieces(&take(), CUTS, 0.0);
    let all: Vec<usize> = (0..got.len()).collect();
    assert_eq!(piece_runs(&got, &all), vec![all.clone()], "{label}: 割った片は 1 つのひと続き");
    assert_eq!(edit_runs(&mut got, &all, &edit), 1);
    assert_same_pieces(label, &got, &want);
}

#[test]
fn ひと続きの片への編集は分割前に掛けてから割ったのと同じ() {
    assert_edit_commutes_with_split("fade-in を端から掛け直す", |e| e.set_edge_fade_in(1.6));
    assert_edit_commutes_with_split("fade-out を端から掛け直す", |e| e.set_edge_fade_out(2.9));
    assert_edit_commutes_with_split("gain", |e| e.gain_db = -6.0);
    // 写像を変える編集 (take を窓へ詰め直してから掛ける) もひと続き全体を 1 つの take として掛かる。
    assert_edit_commutes_with_split("逆再生", |e| {
        e.rebase_take(NATIVE_FPB);
        e.reversed = true;
    });
    assert_edit_commutes_with_split("伸縮 mode", |e| {
        e.rebase_take(NATIVE_FPB);
        e.stretch_mode = StretchMode::Raw;
    });
}

/// 対照: 片ごとに別々に掛けると (旧実装の「全 event に broadcast」) fade は切り口ごとに付き、逆再生は片ごとに
/// 読む向きが変わるので、分割前に掛けたのと一致しない (上のテストの比較器が違いを見分けること)。
#[test]
fn 対照_片ごとに掛けると分割前と一致しない() {
    let mut edited = take();
    edited.set_edge_fade_in(1.6);
    let want = split_pieces(&edited, CUTS, 0.0);
    let mut per_piece = split_pieces(&take(), CUTS, 0.0);
    for e in &mut per_piece {
        e.set_edge_fade_in(1.6_f64.min(e.event_length_beats));
    }
    let same = std::panic::catch_unwind(|| assert_same_pieces("per piece", &per_piece, &want)).is_ok();
    assert!(!same, "片ごとの fade が分割前と区別できない");
}

#[test]
fn 窓に見えている片だけに効き_反対側の窓の片は変わらない() {
    // 1 つの content を [0, 2.25) と [2.25, 4) の 2 つの窓で見る (クリップの分割)。
    let pieces = split_pieces(&take(), [2.25], 0.0);
    let (left, right) = ((0.0, 2.25), (2.25, 4.0));
    assert_eq!(shown_indices(&pieces, left), vec![0]);
    assert_eq!(shown_indices(&pieces, right), vec![1]);

    let mut events = pieces.clone();
    let targets = shown_indices(&events, right);
    edit_runs(&mut events, &targets, |e| {
        e.set_edge_fade_in(0.5);
        e.gain_db = 3.0;
    });
    assert_eq!(events[0], pieces[0], "左の窓の片は 1 bit も変わらない");
    let f = events[1].fade();
    assert_eq!((f.fade_in_beats, f.fade_in_lead_beats), (0.5, 0.0), "fade は右の窓の片の端に付く");
    assert_eq!(events[1].gain_db, 3.0);
    assert_eq!((events[1].id, events[1].take_id), (pieces[1].id, pieces[1].take_id), "安定 id は変わらない");

    // 表示はひと続きから読む: 窓に 2 片並ぶと、fade-in は先頭の片、fade-out は末尾の片、長さは合計。
    let whole = split_pieces(&take(), CUTS, 0.0);
    let all: Vec<usize> = (0..whole.len()).collect();
    let shown = run_fade(&whole, &all).expect("fade");
    let original = take().fade();
    assert!((shown.len_beats - 4.0).abs() <= EPS);
    assert_eq!((shown.fade_in_beats, shown.fade_out_beats), (original.fade_in_beats, original.fade_out_beats));
}

#[test]
fn 分割の片は元の_take_を継ぎ_写した片は別の_take_になる() {
    let mut content = AudioContent { events: vec![take()], next_event_id: 8 };
    let ids = content.split_events(|_| CUTS.to_vec(), 0.0);
    assert_eq!(ids.len(), 4);
    assert!(content.events.iter().all(|e| e.take_key() == 7), "全片が元の event の take");
    let copies: Vec<AudioEvent> = content.events[1..3].to_vec();
    let added = content.adopt_events(copies);
    let (a, b) = (&content.events[added[0]], &content.events[added[1]]);
    assert_ne!(a.take_key(), 7, "写した片は元と編集を共有しない");
    assert_eq!(a.take_key(), b.take_key(), "一緒に写した同じ take の片は同じ take のまま");
    assert!(event_window::joinable(a, b), "一緒に写した隣り合う片はつなげる");
}
