//! handler::range_ops — **時間範囲に対する編集** (`docs/plan_range_selection.md` §8)
//!
//! 範囲操作は「範囲の境界で分割し、範囲部分だけに適用する」 で統一されている。
//! クリップもノートも audio event も同じ規則で、部分的に掛かった要素は分割される。
//!
//! 分割の実体は 2 本だけ:
//! - 窓 (クリップ) の分割 = [`common::model::carve_range`] (非重なり規則と同じ 1 本)
//! - content の分割 = [`common::model::Song::split_content_at`] (共有されていれば CoW)

use crate::state::*;
use common::model::{ClipKey, LaneRef, TimeSelection};

/// 範囲が掛かっているレーンを種類ごとに仕分けた結果。
///
/// ピアノロールの鍵盤行は 1 クリップに複数並ぶので、**クリップ単位にまとめてから**
/// content を触る (content の分割は 1 クリップにつき 1 回で済む)。
#[derive(Default)]
struct LaneGroups {
    tracks: Vec<u32>,
    automation: Vec<common::model::AutomationLaneKey>,
    /// (クリップ, そのクリップで範囲が掛かっている鍵盤の集合)
    key_tracks: Vec<(ClipKey, Vec<u8>)>,
    audio: Vec<ClipKey>,
}

fn group_lanes(sel: &TimeSelection) -> LaneGroups {
    let mut g = LaneGroups::default();
    for lane in &sel.lanes {
        match *lane {
            LaneRef::Track(id) => {
                if !g.tracks.contains(&id) {
                    g.tracks.push(id);
                }
            }
            LaneRef::Automation(key) => {
                if !g.automation.contains(&key) {
                    g.automation.push(key);
                }
            }
            LaneRef::KeyTrack { clip, pitch } => {
                if let Some(entry) = g.key_tracks.iter_mut().find(|(c, _)| *c == clip) {
                    if !entry.1.contains(&pitch) {
                        entry.1.push(pitch);
                    }
                } else {
                    g.key_tracks.push((clip, vec![pitch]));
                }
            }
            LaneRef::AudioLane(clip) => {
                if !g.audio.contains(&clip) {
                    g.audio.push(clip);
                }
            }
        }
    }
    g
}

/// クリップの content を範囲の両端で切り、切り終えた `content_id` を返す。
///
/// 範囲は song 絶対拍で渡し、clip の窓 (`song_to_content_beat`) で content-local へ
/// 換算する。content が共有されていれば [`Song::split_content_at`] が CoW で fork
/// するので、linked clip は影響を受けない。クリップの `content_id` も貼り替える。
/// 返り値は `(content_id, content-local の範囲)`。
fn cut_content_at_range(
    song: &mut common::model::Song,
    key: ClipKey,
    start_beat: f64,
    end_beat: f64,
) -> Option<(common::model::ContentId, f64, f64)> {
    let clip = song.clip_by_key(key)?;
    let (win_start, win_end) = clip.content_window();
    let a = clip.song_to_content_beat(start_beat).max(win_start);
    let b = clip.song_to_content_beat(end_beat).min(win_end);
    if b <= a {
        return None;
    }
    let cid = clip.content_id;
    let cid = song.split_content_at(cid, a);
    let cid = song.split_content_at(cid, b);
    if let Some(clip) = song.clip_by_key_mut(key) {
        clip.content_id = cid;
    }
    Some((cid, a, b))
}

impl AppData {
    /// 範囲がアクティブなときの `Delete`。
    ///
    /// 範囲の境界で分割し、**範囲部分だけ**を削除する。時間は詰めない
    /// (詰めるのは Live の "Delete Time" 相当で、今回は入れていない)。
    /// 選択範囲そのものは残す (Live と同じ — 続けて別の操作ができる)。
    pub(crate) fn apply_delete_time_selection(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            return;
        };
        let follow = self.ui_prefs.automation_follows_clips;
        self.edit_song(|song| {
            let g = group_lanes(&sel);
            let (a, b) = (sel.start_beat, sel.end_beat);
            for track_id in &g.tracks {
                if let Some(track) = song.track_by_id_mut(*track_id) {
                    track.carve_clip_range(a, b, None);
                    // 追従 ON なら、閉じているレーンも含めて同じ範囲を automation にも当てる。
                    if follow {
                        for lane in &mut track.automation_lanes {
                            lane.carve_clip_range(a, b, None);
                        }
                    }
                }
            }
            for key in &g.automation {
                if let Some(lane) = song.automation_lane_by_key_mut(key.track, key.lane) {
                    lane.carve_clip_range(a, b, None);
                }
            }
            for (clip, pitches) in &g.key_tracks {
                if let Some((cid, ca, cb)) = cut_content_at_range(song, *clip, a, b)
                    && let Some(common::model::ClipContent::Midi(midi)) =
                        song.clip_contents.get_mut(&cid)
                {
                    midi.notes.retain(|n| {
                        let inside = n.start_beat >= ca - EPS
                            && n.start_beat + n.duration_beats <= cb + EPS;
                        !(inside && pitches.contains(&n.pitch))
                    });
                }
            }
            for clip in &g.audio {
                if let Some((cid, ca, cb)) = cut_content_at_range(song, *clip, a, b)
                    && let Some(common::model::ClipContent::Audio(audio)) =
                        song.clip_contents.get_mut(&cid)
                {
                    audio.events.retain(|e| {
                        let inside = e.event_start_in_clip_beats >= ca - EPS
                            && e.event_start_in_clip_beats + e.event_length_beats <= cb + EPS;
                        !inside
                    });
                }
            }
        });
    }

    /// 範囲がアクティブなときの `Q` (ミュート)。
    ///
    /// 範囲の境界でクリップを分割し、**範囲部分だけ**の `muted` を反転する
    /// (Live §6.9 "Pressing the 0 key deactivates a selection of material,
    /// even if it contains multiple clips")。 範囲内のクリップが全部ミュート済みなら
    /// 解除、1 つでも鳴っていればミュート。
    pub fn apply_mute_time_selection(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            return;
        };
        // 反転方向は「1 つでも鳴っていればミュート」 (トグルの一般規約)。
        let all_muted = {
            let song = self.song_doc.song();
            let refs = self.selected_clip_refs();
            !refs.is_empty()
                && refs
                    .iter()
                    .all(|k| song.clip_by_key(*k).is_some_and(|c| c.muted))
        };
        let next = !all_muted;
        self.edit_song(|song| {
            let g = group_lanes(&sel);
            for track_id in &g.tracks {
                // 境界で割ってから、範囲に完全に入るクリップだけを反転する。
                split_track_at(song, *track_id, sel.start_beat);
                split_track_at(song, *track_id, sel.end_beat);
                if let Some(track) = song.track_by_id_mut(*track_id) {
                    for clip in &mut track.clips {
                        let (s, e) = clip.song_window();
                        if s >= sel.start_beat - EPS && e <= sel.end_beat + EPS {
                            clip.muted = next;
                        }
                    }
                }
            }
        });
    }
}

/// 1 トラックのクリップを `beat` で分割する (content の切り口も揃える)。
///
/// 範囲操作 (Delete / ミュート / `J`) が範囲の両端で共有する 1 本。
pub(crate) fn split_track_at(song: &mut common::model::Song, track_id: u32, beat: f64) {
    let targets: Vec<(u32, f64, f64, f64, common::model::ContentId)> = song
        .track_by_id(track_id)
        .map(|t| {
            t.clips
                .iter()
                .filter(|c| c.start_beat < beat - EPS && c.start_beat + c.length_beats > beat + EPS)
                .map(|c| {
                    (c.id, c.start_beat, c.length_beats, c.content_offset_beats, c.content_id)
                })
                .collect()
        })
        .unwrap_or_default();
    for (id, start, len, off, cid) in targets {
        let cut = beat - start;
        let cid = song.split_content_at(cid, off + cut);
        let Some(track) = song.track_by_id_mut(track_id) else {
            return;
        };
        let Some(mut right) = track.clip_by_id(id).cloned() else {
            continue;
        };
        if let Some(front) = track.clip_by_id_mut(id) {
            front.content_id = cid;
            front.length_beats = cut;
        }
        right.id = 0;
        right.content_id = cid;
        right.start_beat = beat;
        right.length_beats = len - cut;
        right.content_offset_beats = off + cut;
        right.auto_lipsync = false;
        right.lipsync_gen = 0;
        track.place_clip(right);
    }
}

/// 拍の同一視イプシロン。
const EPS: f64 = 1e-9;

impl AppData {
    /// 範囲を **形のまま** クリップボードへ (`Ctrl+C` / `Ctrl+X`)。
    ///
    /// - 位置は**範囲の先頭**を原点にした相対 — 範囲 4〜12 で素材が 6〜10 にしか
    ///   無ければ先頭 2 拍ぶんの空白も一緒にコピーされ、貼った先でも同じ間隔になる。
    /// - 範囲からはみ出したクリップは**窓を詰めて**取り込む (content は触らない)。
    /// - トラックは範囲が掛かっている行のうち最上段を 0 とした相対 index。
    pub fn copy_time_selection_clip(&self) -> Option<(String, usize)> {
        let sel = self.selection.time.as_ref()?;
        let song = self.song_doc.song();
        let mut resolved: Vec<(usize, common::model::Clip)> = Vec::new();
        for key in self.selected_clip_refs() {
            let (Some(t_idx), Some(clip)) =
                (song.track_index_of(key.track_id), song.clip_by_key(key))
            else {
                continue;
            };
            let (s, e) = clip.song_window();
            let (cs, ce) = (s.max(sel.start_beat), e.min(sel.end_beat));
            if ce <= cs + EPS {
                continue;
            }
            let mut cropped = clip.clone();
            // 窓を範囲へ詰める (左を削った分だけ content 側の起点を進める)。
            cropped.content_offset_beats += cs - s;
            cropped.start_beat = cs;
            cropped.length_beats = ce - cs;
            resolved.push((t_idx, cropped));
        }
        if resolved.is_empty() {
            return None;
        }
        let min_track = resolved.iter().map(|(ti, _)| *ti).min().unwrap_or(0);
        let mut clips = Vec::with_capacity(resolved.len());
        for (ti, c) in &resolved {
            let content = song.clip_contents.get(&c.content_id).cloned().unwrap_or_default();
            let name = song.clip_content_names.get(&c.content_id).cloned();
            clips.push(crate::clipboard::ClipCopy {
                track_offset: (*ti as i64) - (min_track as i64),
                // **範囲の先頭が原点** — 前後の空白がそのまま運ばれる。
                start_beat: c.start_beat - sel.start_beat,
                length_beats: c.length_beats,
                color: c.color,
                muted: c.muted,
                content_id: c.content_id,
                content_offset_beats: c.content_offset_beats,
                content,
                name,
                speaker_id: c.speaker_id,
                singer_name: c.singer_name.clone(),
                style_name: c.style_name.clone(),
                talk: c.talk,
            });
        }
        let count = clips.len();
        let json = crate::clipboard::ClipboardEnvelope::new(
            song.project_id,
            crate::clipboard::ClipboardPayload::Clips(clips),
        )
        .to_json()?;
        Some((json, count))
    }
}

impl AppData {
    /// 範囲がアクティブなときの ←→ = **範囲内の素材をナッジ**する
    /// (Live §6.9 "You can nudge a selection of material using the left and right
    /// arrow keys")。 範囲そのものも同じ量だけ動く。
    ///
    /// 境界でクリップを割ってから、範囲に完全に入るものだけを動かす。 移動先は
    /// 上書き規則で削ってから置く。 追従設定が ON ならそのトラックの automation も
    /// 同じ量だけ動く。
    pub fn nudge_time_selection(&mut self, delta_beats: f64) {
        if delta_beats.abs() <= EPS {
            return;
        }
        let Some(sel) = self.selection.time.clone() else {
            return;
        };
        // ナッジは縦に動かさないので写像は恒等。 **トラック行**だけが対象 —
        // オートメーションレーン行しか掛かっていないトラックのクリップは動かさない。
        let map: Vec<(u32, u32)> = sel.track_row_ids().map(|id| (id, id)).collect();
        // 押しっぱなしのキーリピートを 1 undo step に畳む (ノートの nudge と同じ)。
        self.song_doc.use_stream_scope(StreamGesture::RangeNudge);
        self.move_time_range(sel.start_beat, sel.end_beat, delta_beats, &map);
    }

    /// いま選んでいる範囲が**まさに** `[a, b)` なら、そこに入っているオートメーション
    /// レーン行を返す。 行として明示的に選ばれているので、追従設定に依らず動く / 複製される。
    fn range_automation_lanes(&self, a: f64, b: f64) -> Vec<common::model::AutomationLaneKey> {
        self.selection
            .time
            .as_ref()
            .filter(|t| (t.start_beat - a).abs() < EPS && (t.end_beat - b).abs() < EPS)
            .map_or_else(Vec::new, |t| {
                t.lanes
                    .iter()
                    .filter_map(|l| match l {
                        LaneRef::Automation(k) => Some(*k),
                        _ => None,
                    })
                    .collect()
            })
    }

    /// **範囲の中身を動かす唯一の口** (`docs/plan_range_selection.md` §6)。
    ///
    /// 矢印キーのナッジと、クリップヘッダのドラッグが共有する。 「クリップを動かす」
    /// 操作は無い — 動かせるのは常に範囲で、クリップ 1 つを選んだ状態はその範囲が
    /// たまたまクリップの占有区間と一致しているだけ。
    ///
    /// `track_map` は `(移動元トラック, 行き先トラック)`。 縦に動かさないナッジは
    /// 恒等写像を渡す。 境界でクリップを割ってから、範囲に完全に入るものだけを
    /// 動かし、行き先は上書き規則 ([`Track::place_clip`]) で削ってから置く。
    ///
    /// automation が追従するのは**同じトラックへ動かすとき**だけ。 行き先が別トラック
    /// なら、そこのレーンは別のデバイス / パラメータなので対応付けようがない。
    pub fn move_time_range(
        &mut self,
        a: f64,
        b: f64,
        delta_beats: f64,
        track_map: &[(u32, u32)],
    ) {
        if b <= a + EPS {
            return;
        }
        // 曲頭より前へは動かさない (クランプ量ぶん全体を詰める)。
        let delta = delta_beats.max(-a);
        let vertical = track_map.iter().any(|(from, to)| from != to);
        if delta.abs() <= EPS && !vertical {
            return;
        }
        let follow = self.ui_prefs.automation_follows_clips;
        let same_range = self.selection.time.as_ref().is_some_and(|t| {
            (t.start_beat - a).abs() < EPS && (t.end_beat - b).abs() < EPS
        });
        let auto_lanes = self.range_automation_lanes(a, b);
        let map = track_map.to_vec();
        self.edit_song(move |song| {
            // 1. 元の場所から全部抜く。 行き先を空ける前に抜き終える — 移動元と
            //    行き先が交差していても、抜いたものを削ってしまわない。
            let mut moving: Vec<(u32, common::model::Clip)> = Vec::new();
            for &(from, to) in &map {
                take_range_clips(song, from, to, (a, b), delta, &mut moving);
            }
            // 2. 行き先へ置く (上書き規則は `place_clip` が持つ)。
            for (to, clip) in moving {
                if let Some(track) = song.track_by_id_mut(to) {
                    track.place_clip(clip);
                }
            }
            // 3. automation。 追従 (トラック行ごと) と明示レーンは**同じレーンを
            //    二度ずらさない** (二重にずらすと 2 倍動く)。
            let mut shifted: Vec<common::model::AutomationLaneKey> = Vec::new();
            for &(from, to) in &map {
                if !follow || from != to {
                    continue;
                }
                for key in lane_keys_of(song, from) {
                    if !shifted.contains(&key) {
                        shift_one_lane(song, key, a, b, delta);
                        shifted.push(key);
                    }
                }
            }
            for key in &auto_lanes {
                if !shifted.contains(key) {
                    shift_one_lane(song, *key, a, b, delta);
                    shifted.push(*key);
                }
            }
        });
        // 4. 範囲そのものも一緒に動く (行き先トラックへ貼り替える)。 この区間を
        //    選んでいなかった (= ヘッダを掴んだのが選択外のクリップ) なら、動かした
        //    先を新しい選択にする。
        if same_range {
            if let Some(sel) = self.selection.time.as_mut() {
                sel.start_beat += delta;
                sel.end_beat += delta;
                for lane in &mut sel.lanes {
                    if let LaneRef::Track(id) = lane
                        && let Some(&(_, to)) = track_map.iter().find(|(from, _)| from == id)
                    {
                        *id = to;
                    }
                }
            }
        } else {
            let mut lanes: Vec<LaneRef> = Vec::new();
            for &(_, to) in track_map {
                if !lanes.contains(&LaneRef::Track(to)) {
                    lanes.push(LaneRef::Track(to));
                }
            }
            self.set_time_selection(TimeSelection::new(a + delta, b + delta, lanes));
        }
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }

    /// 範囲の中身を**複製**して行き先へ置く (クリップヘッダの Ctrl+ドラッグ)。
    ///
    /// [`Self::move_time_range`] と対になる 1 本。 元は 1 拍も触らない — 範囲から
    /// はみ出した部分は**窓を詰めて**コピーするので、元クリップを割る必要が無い。
    /// `unique` なら content も fork する (`Ctrl+Shift`)。
    pub fn copy_time_range(
        &mut self,
        a: f64,
        b: f64,
        delta_beats: f64,
        track_map: &[(u32, u32)],
        unique: bool,
    ) {
        if b <= a + EPS {
            return;
        }
        let delta = delta_beats.max(-a);
        let follow = self.ui_prefs.automation_follows_clips;
        let auto_lanes = self.range_automation_lanes(a, b);
        let map = track_map.to_vec();
        let copied_lanes = auto_lanes.clone();
        self.edit_song(move |song| {
            let mut copies: Vec<(u32, common::model::Clip)> = Vec::new();
            for &(from, to) in &map {
                collect_range_copies(song, from, to, (a, b), delta, &mut copies);
            }
            for (to, mut clip) in copies {
                if unique {
                    clip.content_id = song.fork_content(clip.content_id);
                }
                if let Some(track) = song.track_by_id_mut(to) {
                    track.place_clip(clip);
                }
            }
            // automation は移動と同じ規約 — 追従は同じトラックへ置くときだけ、
            // 同じレーンを二度複製しない。
            let mut done: Vec<common::model::AutomationLaneKey> = Vec::new();
            for &(from, to) in &map {
                if !follow || from != to {
                    continue;
                }
                for key in lane_keys_of(song, from) {
                    if !done.contains(&key) {
                        copy_one_lane(song, key, a, b, delta);
                        done.push(key);
                    }
                }
            }
            for key in &copied_lanes {
                if !done.contains(key) {
                    copy_one_lane(song, *key, a, b, delta);
                    done.push(*key);
                }
            }
        });
        // 複製を選択にする (D 連打の後方連鎖と同じ規約)。 行は複製した先の
        // トラック行 + 明示的に選ばれていたオートメーションレーン行。
        let mut lanes: Vec<LaneRef> = Vec::new();
        for &(_, to) in track_map {
            if !lanes.contains(&LaneRef::Track(to)) {
                lanes.push(LaneRef::Track(to));
            }
        }
        for key in auto_lanes {
            if !lanes.contains(&LaneRef::Automation(key)) {
                lanes.push(LaneRef::Automation(key));
            }
        }
        self.set_time_selection(TimeSelection::new(a + delta, b + delta, lanes));
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }

    /// 範囲がアクティブなときの Shift+←→ = **範囲の右端を伸縮**する
    /// (Live §6.9 "hold Shift and use the arrow keys to extend or shorten the selection")。
    /// 左端 (アンカー) は動かない。 幅がゼロ以下になる縮小は無視する。
    pub(crate) fn resize_time_selection(&mut self, delta_beats: f64) {
        let Some(sel) = self.selection.time.as_mut() else {
            return;
        };
        let next_end = sel.end_beat + delta_beats;
        if next_end <= sel.start_beat + EPS {
            return;
        }
        sel.end_beat = next_end;
    }
}

/// トラックの automation レーンの住所を全部並べる (閉じているレーンも含む)。
fn lane_keys_of(
    song: &common::model::Song,
    track_id: u32,
) -> Vec<common::model::AutomationLaneKey> {
    song.track_by_id(track_id)
        .map(|t| {
            t.automation_lanes
                .iter()
                .map(|l| common::model::AutomationLaneKey { track: track_id, lane: l.id })
                .collect()
        })
        .unwrap_or_default()
}

/// `from` トラックの `[a, b)` に掛かるクリップを、範囲で**窓を詰めて**複製し
/// `delta` 先へ置いたものを `out` へ積む (行き先 `to` 付き)。 元は 1 拍も触らない。
fn collect_range_copies(
    song: &common::model::Song,
    from: u32,
    to: u32,
    (a, b): (f64, f64),
    delta: f64,
    out: &mut Vec<(u32, common::model::Clip)>,
) {
    let Some(track) = song.track_by_id(from) else {
        return;
    };
    for c in &track.clips {
        let (s, e) = c.song_window();
        let (cs, ce) = (s.max(a), e.min(b));
        if ce - cs <= EPS {
            continue;
        }
        let mut copy = c.clone();
        copy.id = 0;
        copy.start_beat = cs + delta;
        copy.length_beats = ce - cs;
        copy.content_offset_beats += cs - s;
        // 新規クリップにクロスフェードの張り出しは無い。
        copy.xfade_lead_beats = 0.0;
        copy.xfade_tail_beats = 0.0;
        copy.auto_lipsync = false;
        copy.lipsync_gen = 0;
        out.push((to, copy));
    }
}

/// `from` トラックを `[a, b)` の両端で割り、範囲に**完全に入った**クリップを抜いて
/// `delta` ずらしたものを `out` へ積む (行き先 `to` 付き)。 抜くだけで置きはしない —
/// 移動元と行き先が交差していても、抜いたものを削ってしまわないため。
fn take_range_clips(
    song: &mut common::model::Song,
    from: u32,
    to: u32,
    (a, b): (f64, f64),
    delta: f64,
    out: &mut Vec<(u32, common::model::Clip)>,
) {
    split_track_at(song, from, a);
    split_track_at(song, from, b);
    let Some(track) = song.track_by_id_mut(from) else {
        return;
    };
    track.clips.retain(|c| {
        let (s, e) = c.song_window();
        if s < a - EPS || e > b + EPS {
            return true;
        }
        let mut c = c.clone();
        c.start_beat += delta;
        // 別トラックへ渡すなら id は行き先で採番し直す。 `place_clip` は同じ id を
        // 「置き直し」と見なすので、借りてきた id をそのまま渡すと行き先の無関係な
        // クリップが黙って消える。
        if to != from {
            c.id = 0;
        }
        out.push((to, c));
        false
    });
}

/// 1 レーンの `[a, b)` の automation クリップを `delta` ずらす。
fn shift_one_lane(
    song: &mut common::model::Song,
    key: common::model::AutomationLaneKey,
    a: f64,
    b: f64,
    delta: f64,
) {
    let Some(lane) = song.automation_lane_by_key_mut(key.track, key.lane) else {
        return;
    };
    lane.split_at(a);
    lane.split_at(b);
    let mut moving: Vec<common::model::AutomationClip> = Vec::new();
    lane.clips.retain(|c| {
        let inside = c.start_beat >= a - EPS && c.start_beat + c.length_beats <= b + EPS;
        if inside {
            moving.push(c.clone());
        }
        !inside
    });
    if moving.is_empty() {
        return;
    }
    lane.carve_clip_range(a + delta, b + delta, None);
    for mut c in moving {
        c.start_beat += delta;
        lane.clips.push(c);
    }
}

impl AppData {
    /// 範囲がアクティブなときの ↑↓ = **範囲をレーン方向へ伸ばす**
    /// (`docs/plan_range_selection.md` §3.2)。
    ///
    /// `dir > 0` で今の範囲の一番下の行の**次の行**を、`dir < 0` で一番上の行の
    /// **前の行**を範囲に足す。 行の並びは widget がこのフレームにレイアウトしたもの
    /// (`ui_ephemeral.last_arrange_rows`) を使うので、折り畳み / オートメーションレーンの
    /// 展開状態がそのまま反映される。
    pub(crate) fn extend_time_selection_lanes(&mut self, dir: i32) {
        let rows = self.ui_ephemeral.last_arrange_rows.clone();
        let Some(sel) = self.selection.time.as_mut() else {
            return;
        };
        let index_of = |row: &crate::widgets::arrangement::ArrangementRow| -> Option<usize> {
            let lane = match row.key {
                crate::widgets::arrangement::ArrangementRowKey::Track(id) => LaneRef::Track(id),
                crate::widgets::arrangement::ArrangementRowKey::Lane(k) => LaneRef::Automation(k),
            };
            sel.lanes.iter().position(|l| *l == lane)
        };
        let included: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| index_of(r).is_some())
            .map(|(i, _)| i)
            .collect();
        let Some((&first, &last)) = included.first().zip(included.last()) else {
            return;
        };
        let next = if dir > 0 { last.checked_add(1) } else { first.checked_sub(1) };
        let Some(next) = next.filter(|i| *i < rows.len()) else {
            return;
        };
        let lane = match rows[next].key {
            crate::widgets::arrangement::ArrangementRowKey::Track(id) => LaneRef::Track(id),
            crate::widgets::arrangement::ArrangementRowKey::Lane(k) => LaneRef::Automation(k),
        };
        if !sel.lanes.contains(&lane) {
            sel.lanes.push(lane);
        }
    }
}

/// 1 レーンの `[a, b)` の automation クリップを `offset` 先へ**複製**する
/// (元は据え置き)。 複製先は上書き規則で削ってから置く。
fn copy_one_lane(
    song: &mut common::model::Song,
    key: common::model::AutomationLaneKey,
    a: f64,
    b: f64,
    offset: f64,
) {
    let Some(lane) = song.automation_lane_by_key_mut(key.track, key.lane) else {
        return;
    };
    lane.split_at(a);
    lane.split_at(b);
    let copies: Vec<common::model::AutomationClip> = lane
        .clips
        .iter()
        .filter(|c| c.start_beat >= a - EPS && c.start_beat + c.length_beats <= b + EPS)
        .cloned()
        .collect();
    if copies.is_empty() {
        return;
    }
    lane.carve_clip_range(a + offset, b + offset, None);
    for mut c in copies {
        c.start_beat += offset;
        c.id = lane.next_clip_id.max(1);
        lane.next_clip_id = c.id + 1;
        lane.clips.push(c);
    }
}
