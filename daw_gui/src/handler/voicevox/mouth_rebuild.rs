//! handler::voicevox::mouth_rebuild — 口パクの**生成物を作り直す**純関数群。
//!
//! 入力は「開き口の区間列」だけで、そこから
//! **アレンジの 1 本の `auto_lipsync` clip** (r.md #17/#18) と
//! **列ごとの口パクセル** (r.md #87、設計正本 §3.7) を組み立てる。
//! phoneme query / 発注 / debounce / binding の面倒 (= `AppData` の側) は
//! [`super`] が持ち、ここは `&mut Song` を渡されて目標形へ畳むだけ。
//!
//! **どの関数も冪等**でなければならない — 目標形が現状と一致するなら `false` を
//! 返して `Song` を触らない。load 時の畳み直しや無変更の再生成でも `*` を立てない
//! ため (r.md #9 / `feedback_derived_load_collapse_idempotency`)。

use common::model::Clip;

use crate::app_types::aspect_fit_pip_rect;

/// 2 つの口 ImageEvent 列が「実質同一」か (source_id は厳密一致、 時間/幾何は
/// 1e-6 許容)。 idempotency 判定で exact `==` を使うと、 load 時に
/// `(clip.start + ev.start) - r0` が float 非結合性で元の `ev.start` と bit 単位で
/// 一致せず、 無変更のはずのプロジェクトが開いただけで dirty 化してしまう
/// (r.md #9 違反)。 rebuild が書く field (source_id / 時間 / rect) のみ比較すれば
/// 足りる (他 field は生成時に常に `ImageEvent::default()`)。
pub(super) fn mouth_events_equivalent(
    a: &[common::model::ImageEvent],
    b: &[common::model::ImageEvent],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.source_id == y.source_id
                && (x.event_start_in_clip_beats - y.event_start_in_clip_beats).abs() <= 1e-6
                && (x.event_length_beats - y.event_length_beats).abs() <= 1e-6
                && (x.x - y.x).abs() <= 1e-6
                && (x.y - y.y).abs() <= 1e-6
                && (x.w - y.w).abs() <= 1e-6
                && (x.h - y.h).abs() <= 1e-6
        })
}

/// 口画像の区間 `(start, end, image_id)` 列。座標系は入れ物ごとに違う —
/// アレンジは song 絶対拍、ランチャーのセルは位相拍 (0 = 撃った瞬間)。
pub(super) type MouthSpans = Vec<(f64, f64, u32)>;

/// 列ごとの口画像の区間 `(scene_id, 位相拍の区間列)`。
pub(super) type MouthCellSpans = Vec<(u32, MouthSpans)>;

/// 隙間を埋めた区間列 `filled` (原点 `r0` の座標系) を、生成先 clip / セルの
/// clip-local `ImageEvent` 列にする。
///
/// `build_mouth_events` は rect を全画面 default で返すので、素材寸法から
/// aspect-fit rect を計算して上書きする (立ち絵の他の子レイヤーと収まりを揃える)。
/// アレンジの clip とランチャーのセルで**同じ 1 本**を通す (rect の出し方が
/// 片方だけずれると、撃った瞬間に口の大きさが変わる)。
pub(super) fn mouth_events_for_range(
    song: &common::model::Song,
    filled: &[(f64, f64, u32)],
    r0: f64,
) -> Vec<common::model::ImageEvent> {
    let res = song.video_resolution;
    filled
        .iter()
        .map(|&(s, e, img)| {
            let mut ev = common::model::ImageEvent {
                source_id: img,
                event_start_in_clip_beats: s - r0,
                event_length_beats: e - s,
                ..common::model::ImageEvent::default()
            };
            if let Some(src) = song.media.image_sources.get(&img) {
                let (x, y, w, h) = aspect_fit_pip_rect(res, (src.width, src.height));
                ev.x = x;
                ev.y = y;
                ev.w = w;
                ev.h = h;
            }
            ev
        })
        .collect()
}

/// `spans` (song-absolute の口画像区間) の全体の広がり `(min start, max end)`。
/// 空なら `None`。
pub(super) fn open_span_extent(spans: &[(f64, f64, u32)]) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &(s, e, _) in spans {
        lo = lo.min(s);
        hi = hi.max(e);
    }
    (hi > lo).then_some((lo, hi))
}

/// 口 track (`mouth_track_id`) の `auto_lipsync` clip 群を、 与えられた非重複な
/// 口画像区間 `open` (song-absolute、 start 昇順) と立ち絵 body 範囲から、
/// **閉じ口で隙間を埋めた単一の連続 auto_lipsync clip** に再構築する。
///
/// - r.md #17: 常に「高々 1 本」に畳むので `auto_lipsync` clip が重なり得ない。
/// - r.md #18: `fill_mouth_timeline` が歌/セリフの無い区間 (open の隙間 + 立ち絵
///   範囲の余白) を閉じ口で埋めるので、 立ち絵が映っている間は口が消えない。
///
/// 目標形が現状 (ちょうど 1 本の同一 clip) と一致するなら **何もせず `false`**
/// を返す (idempotent → load 時の collapse や無変更再生成で '*' を付けない)。
/// 実際に clip 集合が変わったら `true`。 `mouth_map` 未設定なら生成不能なので、
/// 残っている auto clip を掃除するだけ (あれば `true`)。
pub(super) fn rebuild_mouth_clip(
    song: &mut common::model::Song,
    mouth_track_id: u32,
    open: MouthSpans,
) -> bool {
    let Some(m_idx) = song.tracks.iter().position(|t| t.id == mouth_track_id) else {
        return false;
    };
    // 閉じ口 image。 mouth_map 未設定 or Closed 未割当なら 0 (= 埋めない)。
    let closed_id = song.tracks[m_idx]
        .mouth_map
        .as_ref()
        .map_or(0, |m| m.resolve(common::model::MouthShape::Closed));

    // 充填範囲: 立ち絵 body が映る範囲 (閉じ口を敷く) ∪ open 区間の広がり。
    // 閉じ口が未割当 (closed_id == 0) のときは body へ広げても埋められないので open のみ。
    let body = if closed_id != 0 {
        common::lipsync::tachie_body_range(song, mouth_track_id)
    } else {
        None
    };
    let fill_range = match (body, open_span_extent(&open)) {
        (Some(b), Some(o)) => Some((b.0.min(o.0), b.1.max(o.1))),
        (Some(b), None) => Some(b),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    };

    // 目標イベント列 (clip-local) を組む。
    let target: Option<(f64, f64, Vec<common::model::ImageEvent>)> =
        fill_range.and_then(|(r0, r1)| {
            let filled = common::lipsync::fill_mouth_timeline(&open, (r0, r1), closed_id);
            if filled.is_empty() {
                return None;
            }
            Some((r0, r1 - r0, mouth_events_for_range(song, &filled, r0)))
        });

    // idempotency: 既に「ちょうど 1 本の auto clip」で目標と一致するなら触らない。
    let auto_positions: Vec<usize> = song.tracks[m_idx]
        .clips
        .iter()
        .enumerate()
        .filter(|(_, c)| c.auto_lipsync)
        .map(|(i, _)| i)
        .collect();
    match &target {
        None => {
            // 目標 = clip 無し。 既存 auto があれば削除、 無ければ no-op。
            if auto_positions.is_empty() {
                return false;
            }
            song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
            song.gc_clip_contents();
            true
        }
        Some((new_start, new_len, new_events)) => {
            if auto_positions.len() == 1 {
                let c = &song.tracks[m_idx].clips[auto_positions[0]];
                // 許容付き比較 (r.md #9: load 時の float 再構成差で無変更を dirty 化しない)。
                let same_geom = (c.start_beat - new_start).abs() <= 1e-6
                    && (c.length_beats - new_len).abs() <= 1e-6;
                let same_events = song
                    .clip_contents
                    .get(&c.content_id)
                    .and_then(|cc| cc.image_events())
                    .is_some_and(|ev| mouth_events_equivalent(ev, new_events));
                if same_geom && same_events {
                    return false;
                }
            }
            // 置換: 既存 auto を全削除 → 単一 clip を追加。
            song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
            let content_id = song.alloc_content(
                common::model::ClipContent::Image(common::model::ImageContent {
                    events: new_events.clone(),
                }),
                "口パク".to_string(),
            );
            let m = &mut song.tracks[m_idx];
            m.place_clip(Clip {
                id: 0,
                start_beat: *new_start,
                length_beats: *new_len,
                content_id,
                color: None,
                auto_lipsync: true,
                // 生成した配置ルールの世代を焼き込む (r.md #39)。load 時に
                // 古い世代を見つけたら一度だけ再生成する。
                lipsync_gen: common::lipsync::PLACEMENT_GEN,
                ..Default::default()
            });
            song.gc_clip_contents();
            true
        }
    }
}

/// 口 track (`mouth_track_id`) の列 `scene_id` の **口パクセル**を再構築する
/// ([`rebuild_mouth_clip`] のランチャー版、設計正本 §3.7)。
///
/// `open` は**位相座標** (0 = セルを撃った瞬間) の開き口区間で、start 昇順・非重複。
/// セルは `start_beat` が常に 0 なので、アレンジ版と違って「範囲」ではなく
/// 長さ 1 つ ([`common::lipsync::mouth_cell_shape`]) で決まる。
///
/// - **ユーザーが手で置いたセルは絶対に触らない** — 列にセルは 1 つしか置けないので、
///   アレンジ側の `place_clip` (重なりを削り取る) と違って上書き = まるごと消滅になる。
/// - `allow_cells` が `false` (= 出力先がグループ行 = セルを置いても永久に鳴らない)
///   なら生成はせず、取り残された生成物の掃除だけ行う。
/// - 目標が現状と一致するなら何もせず `false` (load 時 / 無変更再生成で `*` を
///   立てない、r.md #9)。
/// - 作り直すときも **既存の生成セルの `clip.id` を使い回す** — id が変わると
///   [`common::model::RowPlayback::Launcher`] の指す先が消えたことになり、
///   鳴っている最中に口が消える。
pub(super) fn rebuild_mouth_cell(
    song: &mut common::model::Song,
    mouth_track_id: u32,
    scene_id: u32,
    open: MouthSpans,
    allow_cells: bool,
) -> bool {
    let Some(m_idx) = song.tracks.iter().position(|t| t.id == mouth_track_id) else {
        return false;
    };
    // (clip_id, length, content_id, launch) — 既存の **生成物** セルだけを対象にする。
    let existing: Option<(u32, f64, common::model::ContentId, common::model::LaunchSettings)> =
        match song.tracks[m_idx].session_clip(scene_id) {
            Some(cell) if !cell.clip.auto_lipsync => return false, // 手置きのセル
            Some(cell) => Some((
                cell.clip.id,
                cell.clip.length_beats,
                cell.clip.content_id,
                cell.launch.clone(),
            )),
            None => None,
        };
    let closed_id = song.tracks[m_idx]
        .mouth_map
        .as_ref()
        .map_or(0, |m| m.resolve(common::model::MouthShape::Closed));
    let shape = common::lipsync::mouth_cell_shape(song, mouth_track_id, scene_id);
    // 閉じ口が割り当ててあれば「列で立ち絵が出ている長さ」ぶん敷き詰める。
    // 未割当なら埋められないので開き口の広がりだけ (アレンジ版と同じ規則)。
    let fill_len = if closed_id != 0 {
        shape.map_or(0.0, |s| s.len_beats)
    } else {
        open_span_extent(&open).map_or(0.0, |(_, hi)| hi)
    };
    let launch = shape.map(|s| s.launch()).unwrap_or_default();
    let target = (allow_cells && fill_len > 1e-9)
        .then(|| {
            let filled = common::lipsync::fill_mouth_timeline(&open, (0.0, fill_len), closed_id);
            mouth_events_for_range(song, &filled, 0.0)
        })
        .filter(|events| !events.is_empty());

    let Some(events) = target else {
        // 目標 = セル無し。 取り残された生成物だけ片付ける。
        if existing.is_none() {
            return false;
        }
        song.tracks[m_idx].session_clips.retain(|c| c.scene_id != scene_id);
        song.gc_clip_contents();
        return true;
    };
    if let Some((_, cur_len, content_id, cur_launch)) = &existing {
        let same_geom = (cur_len - fill_len).abs() <= 1e-6;
        let same_events = song
            .clip_contents
            .get(content_id)
            .and_then(|cc| cc.image_events())
            .is_some_and(|ev| mouth_events_equivalent(ev, &events));
        if same_geom && *cur_launch == launch && same_events {
            return false;
        }
    }
    let content_id = song.alloc_content(
        common::model::ClipContent::Image(common::model::ImageContent { events }),
        "口パク".to_string(),
    );
    let track = &mut song.tracks[m_idx];
    let id = match &existing {
        Some((id, ..)) => *id,
        None => track.alloc_clip_id(),
    };
    track.put_session_clip(common::model::SessionClip {
        scene_id,
        clip: Clip {
            id,
            // セルは撃った瞬間が原点 (`SessionClip::clip` の契約)。
            start_beat: 0.0,
            length_beats: fill_len,
            content_id,
            auto_lipsync: true,
            lipsync_gen: common::lipsync::PLACEMENT_GEN,
            ..Default::default()
        },
        launch,
    });
    song.gc_clip_contents();
    true
}

/// マージ済みの開き口区間を、平坦化タイムラインの帯ごとに切り分けて入れ物へ戻す。
///
/// マージ自体は 1 本の拍軸の上でやる (= 複数ソースの重なりが列を跨いでも上位優先で
/// 正しく解ける)。帯は重ならないので、この切り分けはマージ結果を壊さない。
/// 列の区間は帯の原点を引いて**位相座標**へ戻す。
pub(super) fn split_spans_by_container(
    layout: &common::lipsync::LipsyncLayout,
    open: MouthSpans,
) -> (MouthSpans, MouthCellSpans) {
    let mut arrangement: MouthSpans = Vec::new();
    let mut cells: MouthCellSpans = Vec::new();
    for (s, e, img) in open {
        let Some(container) = layout.container_at(s) else {
            // 帯の隙間 = どの入れ物のものでもない (発注時の表と食い違ったときだけ
            // 起きる)。曲頭や別の列へ紛れ込ませるより捨てる方が安全。
            continue;
        };
        let common::lipsync::LipsyncContainer::Scene(scene_id) = container else {
            arrangement.push((s, e, img));
            continue;
        };
        let base = layout.band(scene_id).map_or(0.0, |b| b.base_beat);
        let span = (s - base, e - base, img);
        match cells.iter_mut().find(|(id, _)| *id == scene_id) {
            Some(entry) => entry.1.push(span),
            None => cells.push((scene_id, vec![span])),
        }
    }
    (arrangement, cells)
}

/// 口 track の口パク生成物を **まとめて**作り直す — アレンジの 1 本 + 列ごとのセル。
///
/// `arrangement` は song 絶対拍、`cells` は列ごとの位相拍 (`(scene_id, 区間列)`)。
/// **今回の入力に現れなかった列も必ず通す** — 通さないと、歌のセルを消した列に
/// 口パクセルが取り残される (撃つと歌わないのに口だけ動く)。
pub(super) fn rebuild_mouth_containers(
    song: &mut common::model::Song,
    mouth_track_id: u32,
    arrangement: MouthSpans,
    mut cells: MouthCellSpans,
) -> bool {
    let changed = rebuild_mouth_clip(song, mouth_track_id, arrangement);
    // 出力先がセルを持てる行か。 判定の SSoT は `handler::launcher_cells`
    // (グループトラックは自分のクリップを鳴らさないので、置いたセルは永久に鳴らない)。
    let allow_cells = crate::handler::launcher_cells::row_accepts_cells(
        song,
        crate::event_launcher::LauncherRow::Track(mouth_track_id),
    );
    if !allow_cells && !cells.is_empty() {
        tracing::warn!(
            mouth_track_id,
            "口パクの出力先がセルを置けない行 (グループトラック) なので、ランチャーのセルの口パクは作らない"
        );
    }
    let scene_ids: Vec<u32> = song.scenes.iter().map(|s| s.id).collect();
    let mut cells_changed = false;
    for scene_id in scene_ids {
        let open = cells
            .iter()
            .position(|(id, _)| *id == scene_id)
            .map(|i| cells.swap_remove(i).1)
            .unwrap_or_default();
        cells_changed |= rebuild_mouth_cell(song, mouth_track_id, scene_id, open, allow_cells);
    }
    if cells_changed {
        // セルが消えた列を口 track の行が指したままにしない (歌詞を消した瞬間に
        // 口パクセルが消えるのは正常な経路)。 セルを消したら `normalize_session` を
        // 通すのは launcher 側の編集全部が守っている契約で、ここだけ抜けると
        // 「鳴っているセルが存在しない行」が `.daw` に保存される。
        song.normalize_session();
    }
    changed || cells_changed
}

#[cfg(test)]
mod rebuild_mouth_clip_tests {
    //! r.md #17/#18: `rebuild_mouth_clip` が口 track を「高々 1 本の連続
    //! auto_lipsync clip・隙間は閉じ口」に畳むことの回帰テスト。
    use super::rebuild_mouth_clip;
    use common::model::{Clip, ClipContent, ImageContent, MouthMap, Song, Track};

    fn image_content(song: &mut Song) -> u32 {
        let cid = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Image(ImageContent { events: vec![] }));
        cid
    }

    /// group G(1) + body 立ち絵 track(2, [0,8)) + 口 track(3, closed=99)。
    fn tachie_song(closed: u32) -> Song {
        let mut song = Song::default();
        let body_cid = image_content(&mut song);
        song.tracks = vec![
            Track { id: 1, ..Default::default() },
            Track {
                id: 2,
                parent_group_id: Some(1),
                clips: vec![Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: body_cid,
                    ..Default::default()
                }],
                ..Default::default()
            },
            Track {
                id: 3,
                parent_group_id: Some(1),
                mouth_map: Some(MouthMap { closed, ..Default::default() }),
                ..Default::default()
            },
        ];
        song
    }

    /// 口 track 上の (start, end, source_id) を返す (auto clip の events)。
    fn mouth_triples(song: &Song) -> Vec<(f64, f64, u32)> {
        let m = song.track_by_id(3).unwrap();
        assert_eq!(m.clips.len(), 1, "auto_lipsync clip はちょうど 1 本 (r.md #17)");
        let clip = &m.clips[0];
        assert!(clip.auto_lipsync);
        song.clip_contents
            .get(&clip.content_id)
            .unwrap()
            .image_events()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    clip.content_to_song_beat(e.event_start_in_clip_beats),
                    clip.content_to_song_beat(e.event_start_in_clip_beats) + e.event_length_beats,
                    e.source_id,
                )
            })
            .collect()
    }

    #[test]
    fn single_open_span_is_closed_filled_over_tachie_range() {
        // 開き口 [2,4) img5 のみ。 立ち絵 [0,8) の残りは閉じ口(99)で埋まり、
        // 全体が 1 本の連続 clip [0,8) になる (r.md #18 option1)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        assert_eq!(
            mouth_triples(&song),
            vec![(0.0, 2.0, 99), (2.0, 4.0, 5), (4.0, 8.0, 99)],
        );
    }

    #[test]
    fn rebuild_stamps_the_current_placement_generation() {
        // r.md #39: 再生成した clip には現行の配置ルール世代を焼き込む。これで
        // 「古い世代を見つけたら load 時に一度だけ作り直す」検出が終端する
        // (焼き込みを忘れると毎回 open のたびに再生成 = '*' が付き続ける)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        let clip = &song.track_by_id(3).unwrap().clips[0];
        assert!(clip.auto_lipsync);
        assert_eq!(clip.lipsync_gen, common::lipsync::PLACEMENT_GEN);
        // 世代が現行なので、もう再生成対象にならない。
        assert!(common::lipsync::vocal_tracks_with_outdated_lipsync(&song).is_empty());
    }

    #[test]
    fn rebuild_is_idempotent() {
        // 同じ入力での再構築は clip を作り直さず false を返す (load collapse /
        // 無変更再生成で '*' を付けない = r.md #9 の contract)。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        let before = mouth_triples(&song);
        assert!(
            !rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]),
            "同一入力の再構築は no-op"
        );
        assert_eq!(mouth_triples(&song), before);
    }

    #[test]
    fn closed_unassigned_leaves_gaps_and_no_tachie_extend() {
        // 閉じ口 未割当 (closed=0) → 隙間を埋めず、 立ち絵範囲へも広げない。
        // clip は open span [2,4) だけ (従来どおり隙間は口なし)。
        let mut song = tachie_song(0);
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(2.0, 4.0, 5)]));
        assert_eq!(mouth_triples(&song), vec![(2.0, 4.0, 5)]);
    }

    #[test]
    fn overlapping_legacy_span_collapses_to_single_clip() {
        // 旧 per-clip 生成を模し、 複数 auto clip が既にある状態から呼んでも
        // 1 本に畳まれる (呼び出し側が merge 済みの非重複 span を渡す前提)。
        let mut song = tachie_song(99);
        // 既存の重複 auto clip を 2 本仕込む。
        let c1 = image_content(&mut song);
        let c2 = image_content(&mut song);
        let m_idx = song.tracks.iter().position(|t| t.id == 3).unwrap();
        song.tracks[m_idx].clips = vec![
            Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: c1, auto_lipsync: true, ..Default::default() },
            Clip { id: 2, start_beat: 2.0, length_beats: 4.0, content_id: c2, auto_lipsync: true, ..Default::default() },
        ];
        // 非重複 open span を渡して再構築 → 1 本に。
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(1.0, 3.0, 5)]));
        assert_eq!(song.track_by_id(3).unwrap().clips.len(), 1);
        assert_eq!(
            mouth_triples(&song),
            vec![(0.0, 1.0, 99), (1.0, 3.0, 5), (3.0, 8.0, 99)],
        );
    }

    #[test]
    fn empty_open_fills_whole_body_with_closed() {
        // r.md #18: 開き口が 1 つも無くても、 立ち絵範囲を閉じ口 1 本で覆う。
        let mut song = tachie_song(99);
        assert!(rebuild_mouth_clip(&mut song, 3, Vec::new()));
        assert_eq!(mouth_triples(&song), vec![(0.0, 8.0, 99)]);
        // 同じ状態なら no-op。
        assert!(!rebuild_mouth_clip(&mut song, 3, Vec::new()));
    }

    #[test]
    fn idempotent_after_load_reconstruction_with_fractional_beats() {
        // r.md #9 回帰: body が非整数 beat 始まり + fractional な open だと、 load の
        // span 再構成 `clip.start + ev.start` が float 非結合性で元の値と bit 単位で
        // ズレる。 exact `==` だと無変更でも rebuild → dirty 化していたが、 許容比較
        // (mouth_events_equivalent) で「無変更」と判定して epoch を進めない。
        let mut song = Song::default();
        let body_cid = image_content(&mut song);
        song.tracks = vec![
            Track { id: 1, ..Default::default() },
            Track {
                id: 2,
                parent_group_id: Some(1),
                clips: vec![Clip {
                    id: 1,
                    start_beat: 16.5,
                    length_beats: 8.0,
                    content_id: body_cid,
                    ..Default::default()
                }],
                ..Default::default()
            },
            Track {
                id: 3,
                parent_group_id: Some(1),
                mouth_map: Some(MouthMap { closed: 99, a: 5, ..Default::default() }),
                ..Default::default()
            },
        ];
        // fractional な open 区間で最初の生成。
        assert!(rebuild_mouth_clip(&mut song, 3, vec![(16.5 + 2.3333333, 16.5 + 3.1666667, 5)]));
        // load 相当: mouth clip の **開き口** event を (clip.start + ev.start) で再構成
        // (= normalize_lipsync_clips_on_load と同じ、 closed は除外)。
        let recon: Vec<(f64, f64, u32)> = {
            let m = song.track_by_id(3).unwrap();
            let clip = &m.clips[0];
            song.clip_contents
                .get(&clip.content_id)
                .unwrap()
                .image_events()
                .unwrap()
                .iter()
                .filter(|e| e.source_id != 99)
                .map(|e| {
                    let s = clip.content_to_song_beat(e.event_start_in_clip_beats);
                    (s, s + e.event_length_beats, e.source_id)
                })
                .collect()
        };
        assert!(
            !rebuild_mouth_clip(&mut song, 3, recon),
            "load 再構成後の rebuild は no-op でなければならない (r.md #9)"
        );
    }
}

#[cfg(test)]
mod mouth_cell_tests {
    //! r.md #87: 列 (シーン) ごとの口パクセル。
    //!
    //! ここは静かに壊れる — 生成物が取り残されれば「歌わない列で口だけ動く」、
    //! 冪等でなければ「開くだけで `*`」(r.md #9)、手置きのセルを上書きすれば
    //! **ユーザーのセルが消える** (列にセルは 1 つしか置けないので、アレンジの
    //! 削り取りと違って復元手段が無い)。
    use super::rebuild_mouth_cell;
    use common::model::{
        Clip, ClipContent, ImageContent, LaunchQuantize, LaunchSettings, MidiContent, MouthMap,
        Note, Scene, SessionClip, Song, Track,
    };

    /// group G(1) + 立ち絵 body track(2) + 口 track(3, closed=99) +
    /// 歌 track(4, 出力先 3)。歌 track の列 7 に 4 拍のセルを置く。
    fn cell_song() -> Song {
        let mut song = Song::default();
        song.scenes.push(Scene::new(7));
        let vocal_cid = song.alloc_content_id();
        song.clip_contents.insert(
            vocal_cid,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    start_beat: 1.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    ..Default::default()
                }],
                next_note_id: 2,
            }),
        );
        song.tracks = vec![
            Track { id: 1, ..Default::default() },
            Track { id: 2, parent_group_id: Some(1), ..Default::default() },
            Track {
                id: 3,
                parent_group_id: Some(1),
                mouth_map: Some(MouthMap { closed: 99, a: 5, ..Default::default() }),
                ..Default::default()
            },
            Track {
                id: 4,
                lipsync_target_track: Some(3),
                session_clips: vec![SessionClip {
                    scene_id: 7,
                    clip: Clip {
                        id: 1,
                        start_beat: 0.0,
                        length_beats: 4.0,
                        content_id: vocal_cid,
                        ..Default::default()
                    },
                    launch: LaunchSettings {
                        quantize: LaunchQuantize::Off,
                        looping: false,
                        ..Default::default()
                    },
                }],
                ..Default::default()
            },
        ];
        song
    }

    /// 口 track の列 7 のセルの (start, end, source_id) 列。
    fn cell_triples(song: &Song) -> Vec<(f64, f64, u32)> {
        let cell = song.track_by_id(3).unwrap().session_clip(7).expect("口パクセルがある");
        assert!(cell.clip.auto_lipsync);
        assert_eq!(cell.clip.start_beat, 0.0, "セルの原点は常に 0");
        song.clip_contents
            .get(&cell.clip.content_id)
            .unwrap()
            .image_events()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e.event_start_in_clip_beats,
                    e.event_start_in_clip_beats + e.event_length_beats,
                    e.source_id,
                )
            })
            .collect()
    }

    #[test]
    fn 歌のセルの位相へ閉じ口を敷き詰めて_1_本のセルにする() {
        let mut song = cell_song();
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
        // 歌のセルの長さ (4 拍) いっぱいを覆い、開き口の外は閉じ口。
        assert_eq!(cell_triples(&song), vec![(0.0, 1.0, 99), (1.0, 2.0, 5), (2.0, 4.0, 99)]);
        let cell = song.track_by_id(3).unwrap().session_clip(7).unwrap();
        assert_eq!(cell.clip.length_beats, 4.0);
        assert_eq!(cell.clip.lipsync_gen, common::lipsync::PLACEMENT_GEN);
        // 発火設定は歌のセルから写す (写さないと最初の 1 発で口と歌がズレる)。
        assert_eq!(cell.launch.quantize, LaunchQuantize::Off);
        assert!(!cell.launch.looping);
        // 同じ入力の再構築は no-op (r.md #9)。
        assert!(!rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
    }

    #[test]
    fn 作り直しても_clip_id_は変わらない() {
        // id が変われば `RowPlayback::Launcher` の指す先が消えたことになり、
        // `normalize_session` が行を停止に落とす = 鳴っている最中に口が消える。
        let mut song = cell_song();
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
        let id = song.track_by_id(3).unwrap().session_clip(7).unwrap().clip.id;
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(2.0, 3.0, 5)], true));
        assert_eq!(song.track_by_id(3).unwrap().session_clip(7).unwrap().clip.id, id);
    }

    #[test]
    fn 歌のセルが消えたら生成物も消える() {
        let mut song = cell_song();
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
        let content_id = song.track_by_id(3).unwrap().session_clip(7).unwrap().clip.content_id;
        song.track_by_id_mut(4).unwrap().session_clips.clear();
        assert!(rebuild_mouth_cell(&mut song, 3, 7, Vec::new(), true));
        assert!(song.track_by_id(3).unwrap().session_clip(7).is_none());
        assert!(!song.clip_contents.contains_key(&content_id), "中身も GC される");
        assert!(!rebuild_mouth_cell(&mut song, 3, 7, Vec::new(), true), "2 回目は no-op");
    }

    #[test]
    fn 手で置いたセルは上書きも削除もしない() {
        let mut song = cell_song();
        let hand_cid = song.alloc_content_id();
        song.clip_contents
            .insert(hand_cid, ClipContent::Image(ImageContent { events: vec![] }));
        song.track_by_id_mut(3).unwrap().put_session_clip(SessionClip {
            scene_id: 7,
            clip: Clip { id: 77, length_beats: 2.0, content_id: hand_cid, ..Default::default() },
            launch: LaunchSettings::default(),
        });
        assert!(!rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
        let cell = song.track_by_id(3).unwrap().session_clip(7).unwrap();
        assert_eq!(cell.clip.id, 77);
        assert_eq!(cell.clip.content_id, hand_cid);
    }

    #[test]
    fn セルを持てない行には生成せず取り残しも掃除する() {
        // グループ行など (`row_accepts_cells` が false) は置いても永久に鳴らない。
        let mut song = cell_song();
        assert!(!rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], false));
        assert!(song.track_by_id(3).unwrap().session_clip(7).is_none());
        // 既に生成物があるなら掃除する (行がグループ化された後の取り残し)。
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], true));
        assert!(rebuild_mouth_cell(&mut song, 3, 7, vec![(1.0, 2.0, 5)], false));
        assert!(song.track_by_id(3).unwrap().session_clip(7).is_none());
    }

    #[test]
    fn 入力_fingerprint_はセルの歌詞と発火設定を拾う() {
        // 拾わないと「セルの歌詞を直したのに口パクが再生成されない」で静かに壊れる
        // (debounce 側は fingerprint 一致で再生成を丸ごと skip する)。
        let fp = |song: &Song| crate::state::AppData::lipsync_input_fingerprint(song, 3);
        let base = cell_song();
        let baseline = fp(&base);

        let mut lyric = cell_song();
        let cid = lyric.track_by_id(4).unwrap().session_clips[0].clip.content_id;
        lyric.clip_contents.get_mut(&cid).unwrap().notes_mut().unwrap()[0].lyric =
            Some("ら".into());
        assert_ne!(fp(&lyric), baseline, "セルの歌詞");

        let mut quantize = cell_song();
        quantize.track_by_id_mut(4).unwrap().session_clips[0].launch.quantize =
            LaunchQuantize::Bars(2);
        assert_ne!(fp(&quantize), baseline, "セルのローンチ量子化 (口パクセルへ写す)");

        let mut renamed = cell_song();
        renamed.track_by_id_mut(4).unwrap().name = "別名".into();
        assert_eq!(fp(&renamed), baseline, "非入力の編集では再生成しない");
    }

    #[test]
    fn 歌が無くても立ち絵のセルがあれば閉じ口を置く() {
        // r.md #18 の列版 — 立ち絵が映っている列で口だけ消えない。
        let mut song = cell_song();
        song.track_by_id_mut(4).unwrap().session_clips.clear();
        let body_cid = song.alloc_content_id();
        song.clip_contents
            .insert(body_cid, ClipContent::Image(ImageContent { events: vec![] }));
        song.track_by_id_mut(2).unwrap().put_session_clip(SessionClip {
            scene_id: 7,
            clip: Clip { id: 1, length_beats: 6.0, content_id: body_cid, ..Default::default() },
            launch: LaunchSettings::default(),
        });
        assert!(rebuild_mouth_cell(&mut song, 3, 7, Vec::new(), true));
        assert_eq!(cell_triples(&song), vec![(0.0, 6.0, 99)]);
    }
}

