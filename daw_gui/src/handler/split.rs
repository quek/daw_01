//! handler::split — r.md #132: 分割 (`E` / `Alt+E` / `Shift+E`) と結合 (`J`) の入口
//! (`docs/plan_rmd_132_grid_split.md`)。
//!
//! 3 つの面 (ピアノロールのノート / アレンジのクリップ / オーディオエディタの event) で
//! 「どの位置で切るか」だけを決め、片の作り方と短い片をくっつける規則は common の分割 SSoT
//! (`common::model::content_split`) に任せる。 グリッドの単位は各画面のスナップ設定の
//! [`SnapConfig::grid_unit`](common::snap::SnapConfig::grid_unit) (スナップ OFF でも選んで
//! いる分割)、原点は曲の拍 0。
use std::collections::{BTreeMap, HashMap, HashSet};

use common::model::{ClipContent, ClipKey, ClipWindow, Song, MIN_CLIP_LEN_BEATS, MIN_NOTE_LEN_BEATS};
use common::snap::grid_lines_within;

use crate::app_types::*;
use crate::event_split::{SplitAt, SplitJoinEvent, SplitSurface};
use crate::state::*;

/// 区間 `[start, end)` (拍) → 実際に使う切り口 (昇順)。
type ClipCuts = Box<dyn Fn(f64, f64) -> Vec<f64>>;

/// ピアノロールの切り口の規則 (song 拍の世界で決まり、クリップごとに content-local へ写す)。
#[derive(Clone, Copy)]
enum NoteCut {
    /// song 拍の 1 点。
    Point(f64),
    /// 曲の拍 0 を原点にした、この単位 (拍) のグリッド線。
    Grid(f64),
}

impl NoteCut {
    /// content 原点が song 拍 `origin` のクリップで、ノート `[start, end)` (content-local 拍) を
    /// 切る位置 (content-local 拍)。
    fn positions(self, origin: f64, start: f64, end: f64) -> Vec<f64> {
        match self {
            Self::Point(p) => vec![p - origin],
            Self::Grid(unit) => grid_lines_within(unit, -origin, start, end),
        }
    }
}

impl AppData {
    /// [`SplitJoinEvent`] の処理 (`AppEvent::SplitJoin` の 1 arm)。
    pub(crate) fn handle_split_join_event(&mut self, ev: SplitJoinEvent) {
        match ev {
            SplitJoinEvent::Split { surface: SplitSurface::Notes, at } => self.split_notes(at),
            SplitJoinEvent::Split { surface: SplitSurface::Clips, at } => self.split_clips(at),
            SplitJoinEvent::Join { surface: SplitSurface::Notes } => self.action_join_selected_notes(),
            SplitJoinEvent::Join { surface: SplitSurface::Clips } => self.action_glue_selected_clips(),
        }
    }

    /// ピアノロールのノートを割る。
    ///
    /// - 切り口: `Cursor` = ピアノロール上のポインタ拍 (吸着はピアノロールのスナップ、ポインタが
    ///   grid 外なら再生ヘッド) / `Grid` = ピアノロールのグリッド線すべて。
    /// - 対象 (`E` と `Shift+E` で共通): ポインタ直下のノートが選択外ならその 1 音、選択が
    ///   あれば選択全体、どちらも無ければ表示中の全ノート。
    /// - 最短ノート長より短い片は作らない ([`MIN_NOTE_LEN_BEATS`])。 後ろの片の歌詞は「ー」。
    ///
    /// 選択は範囲 (時間 × 鍵盤行) なので、割っても片はすべて選択されたまま。 同じ content を
    /// 複数のクリップで表示していても content ごとに 1 回だけ切る
    /// ([`Self::resolve_note_entries`]、index がずれた後に別のノートを切らない)。
    pub(crate) fn split_notes(&mut self, at: SplitAt) {
        let Some(cut) = self.note_cut(at) else {
            return;
        };
        let targets = self.split_note_targets();
        if targets.is_empty() {
            self.ui_ephemeral.status_message = "Split: 分割するノートがありません".into();
            return;
        }
        // content ごとに 1 回だけ切る。 linked clip を同時に表示していると同じノートが複数の
        // slot に出るので、ノートごとに「表示していたクリップの content 原点」を全部持ち、
        // それぞれの座標系で見えている位置を切る (= 画面に出ているどの複製でも、カーソル /
        // グリッド線の下で切れる)。
        let shown = self.shown_pianoroll_clips();
        let mut groups: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
        for e in self.resolve_note_entries(&shown, targets.into_iter().map(|id| (id, ()))) {
            let origin = self.clip_start_beat_of(shown[e.slot]);
            groups.entry(e.rep_slot).or_default().push((e.local, origin));
        }
        let mut split_count = 0usize;
        for (slot, frames) in groups {
            split_count += self.split_notes_of_clip(shown[slot], &frames, cut);
        }
        self.ui_ephemeral.status_message = match (split_count, at) {
            (0, SplitAt::Cursor { .. }) => {
                "Split: カーソルがノートの範囲外のため何も分割されませんでした".into()
            }
            (0, SplitAt::Grid) => "Split: グリッド線を跨ぐノートがありません".into(),
            (n, _) => format!("Split: {n} ノートを分割しました"),
        };
    }

    /// クリップ `key` の content で、`frames` = (content 内 index, そのノートを表示していた
    /// クリップの content 原点) のノートを `cut` で割る。 返り値は割ったノートの数
    /// (0 なら undo step を積まない)。
    fn split_notes_of_clip(&mut self, key: ClipKey, frames: &[(usize, f64)], cut: NoteCut) -> usize {
        let mut n = 0usize;
        self.edit_song_checked(|song| {
            let Some(content) = midi_content_in_clip_mut(song, key) else {
                return false;
            };
            // index は編集で動くので、切る前に安定 id へ写す。
            let mut origins: HashMap<u32, Vec<f64>> = HashMap::new();
            for &(i, origin) in frames {
                if let Some(note) = content.notes.get(i) {
                    origins.entry(note.id).or_default().push(origin);
                }
            }
            n = content.split_notes(
                |note| {
                    let end = note.start_beat + note.duration_beats;
                    origins.get(&note.id).map_or_else(Vec::new, |os| {
                        os.iter().flat_map(|&o| cut.positions(o, note.start_beat, end)).collect()
                    })
                },
                MIN_NOTE_LEN_BEATS,
            );
            n > 0
        });
        n
    }

    /// ピアノロールの切り口を解決する。 解決できなければ理由をステータスバーに出して `None`。
    fn note_cut(&mut self, at: SplitAt) -> Option<NoteCut> {
        let cfg = crate::view::snap::piano_roll_snap_config(self);
        let zoom = self.pianoroll_zoom_x();
        let cut = match at {
            SplitAt::Cursor { snap } => self
                .cur
                .peph
                .pianoroll_hover_beat_song_raw
                .or_else(|| self.cur.transport.playhead_beat.map(f64::from))
                .map(|raw| NoteCut::Point(cfg.snap_beat(raw, !snap, zoom))),
            SplitAt::Grid => cfg.grid_unit(zoom).map(NoteCut::Grid),
        };
        if cut.is_none() {
            self.ui_ephemeral.status_message = match at {
                SplitAt::Cursor { .. } => {
                    "Split: マウスをピアノロールに置くか再生中に E を押してください".into()
                }
                SplitAt::Grid => "Split: グリッドの分割が選ばれていません".into(),
            };
        }
        cut
    }

    /// 分割するノート (packed id)。 `E` / `Shift+E` 共通の規則 — ポインタ直下のノートが選択に
    /// 入っていなければその 1 音、選択があれば選択全体 (velocity lane の掴み方と同じ)、
    /// どちらも無ければ表示中クリップの **全ノート** (切り口を跨ぐものだけが実際に割れる)。
    fn split_note_targets(&self) -> Vec<u32> {
        let selected = self.selected_note_ids();
        match self.cur.peph.pianoroll_hover_note {
            Some(id) if !selected.contains(&id) => vec![id],
            _ if !selected.is_empty() => selected,
            _ => {
                let song = self.cur.song_doc.song();
                self.shown_pianoroll_clips()
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, key)| song.clip_by_key(*key).map(|clip| (slot, clip)))
                    .flat_map(|(slot, clip)| {
                        (0..song.clip_notes(clip).len()).map(move |idx| Self::pack_note_id(slot, idx))
                    })
                    .collect()
            }
        }
    }

    /// アレンジのクリップを割る。 オーディオエディタの波形の上にポインタがあるときは、
    /// 開いているクリップの event を割る ([`Self::split_audio_editor_events`])。
    ///
    /// - 切り口: `Cursor` = アレンジのポインタ拍 (`snap` でアレンジのスナップに吸着、ポインタが
    ///   キャンバス外なら再生ヘッド) / `Grid` = アレンジのグリッド線すべて。
    /// - 対象 (`E` と `Shift+E` で共通): ポインタ直下のクリップ、無ければ選択クリップ。
    /// - `Grid` はクリップの最短長より短い片を作らない ([`MIN_CLIP_LEN_BEATS`])。
    ///
    /// 片は全部選択される。 content は切り口で 1 回だけ切り、片は同じ content を別の窓で
    /// 見る ([`split_clip_window`])。
    pub(crate) fn split_clips(&mut self, at: SplitAt) {
        if self.cur.peph.audio_editor_clip.is_some()
            && self.cur.peph.audio_editor_hover_beat_in_clip.is_some()
        {
            self.split_audio_editor_events(at);
            return;
        }
        let Some(cuts_for) = self.clip_cuts(at) else {
            return;
        };
        let Some(targets) = self.split_clip_targets() else {
            self.ui_ephemeral.status_message =
                "Split: clip にマウスを乗せるか clip を選択してください".into();
            return;
        };
        let mut split_count = 0usize;
        let mut pieces: Vec<ClipKey> = Vec::new();
        for key in targets {
            let Some((start, end)) = self.cur.song_doc.song().clip_by_key(key).map(|c| c.song_window())
            else {
                continue;
            };
            let cuts = cuts_for(start, end);
            if cuts.is_empty() {
                continue;
            }
            let mut got: Vec<ClipKey> = Vec::new();
            self.edit_song_checked(|song| {
                got = split_clip_window(song, key, &cuts);
                !got.is_empty()
            });
            if got.len() > 1 {
                split_count += 1;
                pieces.extend(got);
            }
        }
        if split_count == 0 {
            self.ui_ephemeral.status_message = match at {
                SplitAt::Cursor { .. } => {
                    "Split: カーソルが clip 範囲外のため何も分割されませんでした".into()
                }
                SplitAt::Grid => "Split: グリッド線を跨ぐ clip がありません".into(),
            };
            return;
        }
        self.select_new_clips(&pieces);
        self.ui_ephemeral.status_message = format!("Split: {split_count} clip を分割しました");
    }

    /// アレンジの切り口 = 「クリップの区間 `[start, end)` (song 拍) → 使う切り口」 の関数。
    /// 解決できなければ理由をステータスバーに出して `None`。
    fn clip_cuts(&mut self, at: SplitAt) -> Option<ClipCuts> {
        let peph = &self.cur.peph;
        let playhead = self.cur.transport.playhead_beat.map(f64::from);
        let cut = match at {
            SplitAt::Cursor { snap: true } => {
                peph.arrangement_hover_beat.or(peph.arrangement_hover_beat_raw).or(playhead)
            }
            SplitAt::Cursor { snap: false } => {
                peph.arrangement_hover_beat_raw.or(peph.arrangement_hover_beat).or(playhead)
            }
            SplitAt::Grid => None,
        };
        let unit = crate::view::snap::arrange_snap_config(self)
            .grid_unit(self.cur.view.arrange_zoom_x.max(1.0));
        match (at, cut, unit) {
            (SplitAt::Cursor { .. }, Some(p), _) => {
                Some(Box::new(move |s, e| common::model::split_boundaries(s, e, [p], 0.0)))
            }
            (SplitAt::Grid, _, Some(unit)) => Some(Box::new(move |s, e| {
                let lines = grid_lines_within(unit, 0.0, s, e);
                common::model::split_boundaries(s, e, lines, MIN_CLIP_LEN_BEATS)
            })),
            (SplitAt::Cursor { .. }, None, _) => {
                self.ui_ephemeral.status_message =
                    "Split: マウスを arrangement に置くか再生中に E を押してください".into();
                None
            }
            (SplitAt::Grid, _, None) => {
                self.ui_ephemeral.status_message = "Split: グリッドの分割が選ばれていません".into();
                None
            }
        }
    }

    /// 分割するアレンジのクリップ。 ポインタ直下のクリップ、無ければ選択クリップ
    /// (範囲が立っているとき)。 ランチャーのセルは曲の時間軸に居ないので含めない。
    fn split_clip_targets(&self) -> Option<Vec<ClipKey>> {
        let targets = match self.cur.peph.arrangement_hover_clip {
            Some(hover) => vec![hover],
            None if self.cur.selection.time.is_some() => self.selected_clip_refs(),
            None => return None,
        };
        let song = self.cur.song_doc.song();
        Some(targets.into_iter().filter(|k| !song.is_session_clip(*k)).collect())
    }

    /// オーディオエディタで、ポインタが乗っている event を割る。
    ///
    /// - 切り口: `Cursor` = ポインタの拍 (吸着なし) / `Grid` = アレンジのスナップ設定の
    ///   グリッド線 (単位はオーディオエディタの表示倍率で決まる)。
    /// - `Cursor` は後ろの片を、`Grid` は全片を選択する。
    fn split_audio_editor_events(&mut self, at: SplitAt) {
        let (Some(target), Some(hover)) =
            (self.cur.peph.audio_editor_clip, self.cur.peph.audio_editor_hover_beat_in_clip)
        else {
            return;
        };
        let unit = crate::view::snap::arrange_snap_config(self)
            .grid_unit(self.cur.peph.audio_editor_zoom_x.max(1.0));
        let Some((content_id, origin)) = self
            .cur
            .song_doc
            .song()
            .clip_by_key(target)
            .map(|c| (c.content_id, c.content_origin_beat()))
        else {
            return;
        };
        // 切るのは「ポインタが乗っている event」。 1 点で切るときは切り口が端に来ない event
        // (= 内側に乗っている) を選ぶ。
        let under = |s: f64, e: f64| match at {
            SplitAt::Cursor { .. } => hover > s + 1e-9 && hover < e - 1e-9,
            SplitAt::Grid => hover >= s && hover < e,
        };
        let event_id = match self.cur.song_doc.song().clip_contents.get(&content_id) {
            Some(ClipContent::Audio(audio)) => audio
                .events
                .iter()
                .find(|e| {
                    under(e.event_start_in_clip_beats, e.event_start_in_clip_beats + e.event_length_beats)
                })
                .map(|e| e.id),
            _ => return,
        };
        let Some(event_id) = event_id else {
            self.ui_ephemeral.status_message =
                "Split: カーソル位置に分割可能な event がありません".into();
            return;
        };
        let (cuts, min_piece): (ClipCuts, f64) = match (at, unit) {
            (SplitAt::Cursor { .. }, _) => (Box::new(move |_, _| vec![hover]), 0.0),
            (SplitAt::Grid, Some(unit)) => {
                (Box::new(move |s, e| grid_lines_within(unit, -origin, s, e)), MIN_CLIP_LEN_BEATS)
            }
            (SplitAt::Grid, None) => {
                self.ui_ephemeral.status_message = "Split: グリッドの分割が選ばれていません".into();
                return;
            }
        };
        let mut pieces: Vec<u32> = Vec::new();
        self.edit_song_checked(|song| {
            let Some(ClipContent::Audio(audio)) = song.clip_contents.get_mut(&content_id) else {
                return false;
            };
            pieces = audio.split_events(
                |e| {
                    if e.id == event_id {
                        cuts(e.event_start_in_clip_beats, e.event_start_in_clip_beats + e.event_length_beats)
                    } else {
                        Vec::new()
                    }
                },
                min_piece,
            );
            !pieces.is_empty()
        });
        if pieces.is_empty() {
            self.ui_ephemeral.status_message = match at {
                SplitAt::Cursor { .. } => "Split: カーソル位置に分割可能な event がありません".into(),
                SplitAt::Grid => "Split: グリッド線を跨ぐ event がありません".into(),
            };
            return;
        }
        // 選択は index (範囲からの導出) なので、編集後の並びで id を引き直す。
        let select: HashSet<u32> = match at {
            // 1 点で切ったら後ろの片 (分割直後に続きを編集することが多い、Reaper / Bitwig 流)。
            SplitAt::Cursor { .. } => pieces.iter().copied().filter(|&id| id != event_id).collect(),
            SplitAt::Grid => pieces.iter().copied().collect(),
        };
        let indices: Vec<usize> = match self.cur.song_doc.song().clip_contents.get(&content_id) {
            Some(ClipContent::Audio(a)) => a
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| select.contains(&e.id))
                .map(|(i, _)| i)
                .collect(),
            _ => Vec::new(),
        };
        self.set_audio_event_selection(&indices);
        self.ui_ephemeral.status_message = "Split: event を分割しました".into();
        if self.cur.peph.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }
}

/// アレンジのクリップ `key` を song 拍の切り口 `cuts` (昇順・窓の内側、 [`common::model::split_boundaries`]
/// を通したもの) で割り、片の `ClipKey` を先頭から返す (先頭 = 元のクリップ。 割れなければ空)。
///
/// **content は切り口で 1 回だけ切り、窓を割る** (`docs/plan_range_selection.md` §10)。 跨ぐ
/// note / event は [`Song::split_content_at_points`] が割る (共有されている MIDI は 1 回だけ fork
/// するので linked clip は無傷。 時間軸を持つ event は切っても鳴り方が変わらないので共有のまま)。
/// 片は**同じ content を別の窓で見る**ので、窓の外に隠れていた
/// 素材は失われない。 後ろの片は口パク自動生成の再生成対象から外す (手で割った片を
/// 再生成で消さない)。
///
/// `E` / `Shift+E` と範囲操作の両端 (`handler::range_ops::split_track_at`) が共有する 1 本。
pub(crate) fn split_clip_window(song: &mut Song, key: ClipKey, cuts: &[f64]) -> Vec<ClipKey> {
    let Some(clip) = song.clip_by_key(key).cloned() else {
        return Vec::new();
    };
    let (start, end) = clip.song_window();
    let Some(&first) = cuts.first() else {
        return Vec::new();
    };
    let off = clip.content_offset_beats;
    // 切る位置は **content-local** 拍 (= 窓の offset ぶん進んだ位置)。
    let local: Vec<f64> = cuts.iter().map(|c| off + (c - start)).collect();
    let content_id = song.split_content_at_points(clip.content_id, &local);
    let Some(track) = song.track_by_id_mut(key.track_id) else {
        return Vec::new();
    };
    if let Some(front) = track.clip_by_id_mut(key.clip_id) {
        front.content_id = content_id;
        front.length_beats = first - start;
        front.clear_overhang(false, true);
    }
    let mut pieces = vec![key];
    for (i, &at) in cuts.iter().enumerate() {
        let next = cuts.get(i + 1).copied();
        let mut piece = common::model::Clip {
            id: 0,
            content_id,
            start_beat: at,
            length_beats: next.unwrap_or(end) - at,
            content_offset_beats: off + (at - start),
            auto_lipsync: false,
            lipsync_gen: 0,
            ..clip.clone()
        };
        // 切り口の端はクロスフェードの張り出しを継がない (外側の端 = 末尾の片の尻だけが継ぐ)。
        piece.clear_overhang(true, next.is_some());
        let clip_id = track.place_clip(piece);
        pieces.push(ClipKey { track_id: key.track_id, clip_id });
    }
    pieces
}
