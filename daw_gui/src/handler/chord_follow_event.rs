//! handler::chord_follow_event — コードトラックへの追従の設定の入口 (`AppEvent::ChordFollow`、r.md #157)。
//!
//! 追従の評価は `common::chord_follow` が SSoT。ここは設定 (`Song::chord_follow_enabled` /
//! `Track::follow_chords`) を `edit_song` へ通すだけで、engine / VOICEVOX / 印はフレーム末の同期と描画が
//! Song から読み直す。「鳴る音で確定」はピアノロールの薄い枠と同じ読み口 (`handler::chord_follow_view`) で
//! 書き換える。

use common::model::{ChordFollow, Song};

use crate::app::ClipKey;
use crate::event::AppEvent;
use crate::event_chord::{ChordFollowEvent, FreezeTarget};
use crate::handler::chord_follow_view::ClipSoundingKeys;
use crate::state::AppData;

impl AppData {
    /// [`ChordFollowEvent`] の処理 (`AppEvent::ChordFollow` の 1 arm)。変化が無ければ undo を積まない。
    pub(crate) fn handle_chord_follow_event(&mut self, ev: ChordFollowEvent) {
        self.edit_song_checked(|song| match ev {
            ChordFollowEvent::SetEnabled(on) => {
                let changed = song.chord_follow_enabled != on;
                song.chord_follow_enabled = on;
                changed
            }
            ChordFollowEvent::SetTracks { track_ids, mode } => {
                let targets = follow_targets(song, &track_ids);
                let cleared = clear_group_modes(song, &track_ids);
                set_modes(song, &targets, mode) | cleared
            }
            ChordFollowEvent::FreezePitch { targets } => freeze_clips(song, &targets),
            ChordFollowEvent::FreezeTracks { track_ids } => {
                let track_ids = follow_targets(song, &track_ids);
                let targets: Vec<FreezeTarget> = song
                    .tracks
                    .iter()
                    .filter(|t| track_ids.contains(&t.id))
                    .flat_map(|t| {
                        t.clips.iter().map(move |c| FreezeTarget { track_id: t.id, clip_id: c.id, note_ids: Vec::new() })
                    })
                    .collect();
                // 確定してから追従を切る (切った後では鳴る音が書いた音になって確定するものが無い)。確定するのは
                // アレンジの MIDI クリップだけ — セル / オーディオは確定できず、追従が切れて書いた音に戻る
                // (`ChordFollowEvent::FreezeTracks` の doc)。
                let frozen = freeze_clips(song, &targets);
                set_modes(song, &track_ids, ChordFollow::Off) | frozen
            }
        });
    }

    /// クリップの右クリック「鳴る音で確定」: 対象は、右クリックしたクリップが選択に入っていれば選択の
    /// クリップ全部、でなければそのクリップだけ (ノートは全部。「コードを抽出…」と同じ読み方)。
    pub(crate) fn freeze_clip_pitch(&mut self, target: ClipKey) {
        let selected = self.selected_clip_refs();
        let keys = if selected.contains(&target) { selected } else { vec![target] };
        let targets = keys
            .into_iter()
            .map(|k| FreezeTarget { track_id: k.track_id, clip_id: k.clip_id, note_ids: Vec::new() })
            .collect();
        self.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezePitch { targets }));
    }

    /// ピアノロールの「確定」: ノートを選んでいればそのノートだけ、選んでいなければ表示中のクリップのノート全部。
    pub(crate) fn freeze_shown_pitch(&mut self) {
        let shown = self.shown_pianoroll_clips();
        let song = self.cur.song_doc.song();
        let selected = self.selected_note_ids();
        let targets: Vec<FreezeTarget> = shown
            .iter()
            .enumerate()
            .filter_map(|(slot, k)| {
                let clip = song.track_by_id(k.track_id)?.clip_by_id(k.clip_id)?;
                let notes = song.clip_notes(clip);
                // packed id (表示 slot + content 内の index) → その content の `Note::id`。
                let note_ids: Vec<u32> = selected
                    .iter()
                    .filter(|id| Self::note_id_clip_slot(**id) == slot)
                    .filter_map(|id| notes.get(Self::note_id_local_index(*id)).map(|n| n.id))
                    .collect();
                (selected.is_empty() || !note_ids.is_empty())
                    .then_some(FreezeTarget { track_id: k.track_id, clip_id: k.clip_id, note_ids })
            })
            .collect();
        self.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezePitch { targets }));
    }

    /// ピアノロールに「確定」を出すか = 表示中のアレンジのクリップに、追従するトラックのものがあるか。
    #[must_use]
    pub(crate) fn can_freeze_shown_pitch(&self) -> bool {
        let song = self.cur.song_doc.song();
        self.shown_pianoroll_clips().iter().any(|k| {
            // セルは確定できない (`ClipSoundingKeys::of`)。`Track::clip_by_id` はセルも返すので、アレンジの列を見る。
            song.track_by_id(k.track_id).is_some_and(|t| t.clip_index_by_id(k.clip_id).is_some())
                && !song.track_chord_follow(k.track_id).is_off()
        })
    }

    /// トラックの右クリックメニューでチェックを付ける追従モード: 設定が入るトラック ([`follow_targets`]: グループは
    /// 子孫) が全部同じモードならそのモード、混ざっている / 入るトラックが無いなら `None` (どれにも付けない)。
    #[must_use]
    pub(crate) fn track_menu_chord_follow(&self, track_id: u32) -> Option<ChordFollow> {
        let song = self.cur.song_doc.song();
        if !song.track_has_children(track_id) {
            return song.track_by_id(track_id).map(|t| t.follow_chords);
        }
        let targets = follow_targets(song, &[track_id]);
        let mut modes = song.tracks.iter().filter(|t| targets.contains(&t.id)).map(|t| t.follow_chords);
        let first = modes.next()?;
        modes.all(|m| m == first).then_some(first)
    }
}

/// 追従の設定 / 確定を入れるトラック: `track_ids` のうち **グループ (子を持ち自分のクリップを鳴らさない
/// [`Song::track_is_pure_group`]) は子孫に展開**し、グループ自身には入れない (追従はクリップのノート / 音源に
/// 効くので、グループに入れても子の音は変わらず印だけが出る)。入れ子は `parent_group_id` を下へ辿る (#130 の
/// 移調の lineage と同じ親子の向き。循環しても各トラックを 1 回しか見ないので止まる)。順は `song.tracks` の並び。
fn follow_targets(song: &Song, track_ids: &[u32]) -> Vec<u32> {
    let mut picked: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut stack: Vec<u32> = track_ids.to_vec();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if song.track_is_pure_group(id) {
            stack.extend(song.tracks.iter().filter(|t| t.parent_group_id == Some(id)).map(|t| t.id));
        } else {
            picked.insert(id);
        }
    }
    song.tracks.iter().map(|t| t.id).filter(|id| picked.contains(id)).collect()
}

/// `track_ids` のうちグループ自身に残っているモード (グループになる前に入れた設定) を Off にする。グループの値は
/// 音に効かず、印だけが出て、メニューの設定は子孫へ行くので、ここで消さないと消す手段が無い。
fn clear_group_modes(song: &mut Song, track_ids: &[u32]) -> bool {
    let groups: Vec<u32> = track_ids.iter().copied().filter(|id| song.track_is_pure_group(*id)).collect();
    set_modes(song, &groups, ChordFollow::Off)
}

fn set_modes(song: &mut Song, track_ids: &[u32], mode: ChordFollow) -> bool {
    let mut changed = false;
    for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
        changed |= t.follow_chords != mode;
        t.follow_chords = mode;
    }
    changed
}

/// `targets` のクリップのノート (`note_ids` が空なら全部) の書いた音を、今の追従結果で書き換える。変わったら `true`。
///
/// - 鳴る音は **確定前の Song で全部解いてから** 書き換える (素材のコードの検出が書き換えの途中の値を見ない)。
/// - 他のクリップ (別の位置の linked clip / セル) と content を共有していれば、先に複製してから書き換える
///   (鳴る音は位置のコードで決まるので、共有したまま書くと別の位置のクリップの音が変わる)。
/// - 追従で 0..=127 を外れて鳴っていなかったノートは、音程を残してミュートする (確定しても鳴らないまま)。
/// - トラックの追従モードは変えない。最寄りのコード音 / スケールは確定した音がそのまま鳴る。平行移動 / ベースは
///   確定した音から素材のコードを検出し直すので、検出できないクリップ (単音) はもう一度動く — トラック全体を
///   確定して追従を切るのは [`ChordFollowEvent::FreezeTracks`]。
fn freeze_clips(song: &mut Song, targets: &[FreezeTarget]) -> bool {
    type Plan = ((u32, u32), Vec<(u32, Option<u8>)>);
    let plans: Vec<Plan> = targets
        .iter()
        .filter_map(|FreezeTarget { track_id, clip_id, note_ids }| {
            let (track_id, clip_id) = (*track_id, *clip_id);
            let clip = song.track_by_id(track_id)?.clip_by_id(clip_id)?;
            let keys = ClipSoundingKeys::of(song, track_id, clip);
            if !keys.is_active() {
                return None;
            }
            let notes = song.clip_contents.get(&clip.content_id)?.notes()?;
            let changes: Vec<(u32, Option<u8>)> = notes
                .iter()
                .filter(|n| note_ids.is_empty() || note_ids.contains(&n.id))
                .filter_map(|n| {
                    let key = keys.key(n);
                    (key != Some(n.pitch) && !(key.is_none() && n.muted)).then_some((n.id, key))
                })
                .collect();
            (!changes.is_empty()).then_some(((track_id, clip_id), changes))
        })
        .collect();
    if plans.is_empty() {
        return false;
    }
    for ((track_id, clip_id), changes) in plans {
        let Some(content_id) = song.track_by_id(track_id).and_then(|t| t.clip_by_id(clip_id)).map(|c| c.content_id)
        else {
            continue;
        };
        let content_id = if content_users(song, content_id) > 1 {
            let forked = song.fork_content(content_id);
            if let Some(clip) = song.track_by_id_mut(track_id).and_then(|t| t.clip_by_id_mut(clip_id)) {
                clip.content_id = forked;
            }
            forked
        } else {
            content_id
        };
        let Some(notes) = song.clip_contents.get_mut(&content_id).and_then(|c| c.notes_mut()) else { continue };
        for n in notes.iter_mut() {
            match changes.iter().find(|(id, _)| *id == n.id).map(|(_, key)| *key) {
                Some(Some(key)) => n.pitch = key,
                Some(None) => n.muted = true,
                None => {}
            }
        }
    }
    true
}

/// content `content_id` を指すクリップ (アレンジとランチャーのセル、全トラック) の数。
fn content_users(song: &Song, content_id: common::model::ContentId) -> usize {
    song.tracks
        .iter()
        .flat_map(|t| t.clips.iter().chain(t.session_clips.iter().map(|c| &c.clip)))
        .filter(|c| c.content_id == content_id)
        .count()
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordEvent, ChordFollow, ChordKind, Clip, ClipContent, MidiContent, Note};

    use crate::event::AppEvent;
    use crate::event_chord::{ChordFollowEvent, FreezeTarget};
    use crate::state::AppData;

    /// トラック 1 本目に、C の 3 和音の content を共有する linked clip 2 つ (拍 0 と 8) を置き、Am [0,4) / F [8,12)
    /// の箱とモード `mode` を入れる。`(track id, [clip id; 2])`。
    fn linked_triads(app: &mut AppData, mode: ChordFollow) -> (u32, [u32; 2]) {
        let track_id = app.cur.song_doc.song().tracks[0].id;
        app.edit_song(|song| {
            let content_id = song.alloc_content_id();
            let notes = [60u8, 64, 67]
                .iter()
                .zip(1u32..)
                .map(|(&pitch, id)| Note { id, start_beat: 0.0, duration_beats: 2.0, pitch, velocity: 100, lyric: None, muted: false })
                .collect();
            song.clip_contents.insert(content_id, ClipContent::Midi(MidiContent { notes, next_note_id: 4 }));
            let track = song.track_by_id_mut(track_id).expect("track");
            track.follow_chords = mode;
            for (id, start) in [(901, 0.0), (902, 8.0)] {
                track.clips.push(Clip { id, start_beat: start, length_beats: 4.0, content_id, ..Clip::default() });
            }
            for (start, root, kind) in [(0.0, 9, ChordKind::Minor), (8.0, 5, ChordKind::Major)] {
                song.place_chord(ChordEvent { id: 0, start_beat: start, len_beats: 4.0, chord: Chord::new(root, kind), color: None });
            }
        });
        (track_id, [901, 902])
    }

    /// クリップの、書いた音と鳴る音 (どちらも昇順)。
    fn written_and_sounding(app: &AppData, track_id: u32, clip_id: u32) -> (Vec<u8>, Vec<u8>) {
        let song = app.cur.song_doc.song();
        let clip = song.track_by_id(track_id).and_then(|t| t.clip_by_id(clip_id)).expect("clip");
        let notes = song.clip_contents[&clip.content_id].notes().expect("midi");
        let keys = app.clip_sounding_keys(track_id, clip_id).expect("keys");
        let mut written: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
        let mut sounding: Vec<u8> = notes.iter().filter_map(|n| keys.key(n)).collect();
        written.sort_unstable();
        sounding.sort_unstable();
        (written, sounding)
    }

    /// トラックを鳴る音で確定すると、linked clip は位置ごとの鳴る音を別々の content に書き、追従が切れ、
    /// **確定の前後で鳴る音が同じ**。undo 1 回で元に戻る。
    #[test]
    fn freezing_a_track_keeps_the_sound_and_unlinks_clips_heard_at_different_chords() {
        let mut app = crate::test_support::headless_app();
        let (track_id, clips) = linked_triads(&mut app, ChordFollow::Narrow);
        let before = clips.map(|c| written_and_sounding(&app, track_id, c).1);
        // 最寄りのコード音: Am の上は G → A、F の上は E → F / G → A。
        assert_eq!(before, [vec![60, 64, 69], vec![60, 65, 69]]);
        let depth = app.cur.song_doc.undo_depth();

        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezeTracks { track_ids: vec![track_id] }));
        let song = app.cur.song_doc.song();
        assert_eq!(song.track_by_id(track_id).expect("track").follow_chords, ChordFollow::Off);
        let contents = clips.map(|c| song.track_by_id(track_id).and_then(|t| t.clip_by_id(c)).expect("clip").content_id);
        assert_ne!(contents[0], contents[1], "位置ごとに鳴る音が違うので content を分ける");
        assert_eq!(clips.map(|c| written_and_sounding(&app, track_id, c)), before.clone().map(|s| (s.clone(), s)));
        assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "確定は 1 undo step");

        app.handle_event(AppEvent::Undo);
        let [a, b] = clips.map(|c| written_and_sounding(&app, track_id, c));
        assert_eq!((a.0, b.0), (vec![60, 64, 67], vec![60, 64, 67]), "undo で書いた音に戻る");
        assert_eq!([a.1, b.1], before, "undo で追従も戻る");
    }

    /// クリップの一部のノートだけを確定すると、そのノートだけが鳴る音に書き換わり、共有していた linked clip は
    /// 元の書いた音のまま。トラックの追従は残る。
    #[test]
    fn freezing_selected_notes_rewrites_only_them() {
        let mut app = crate::test_support::headless_app();
        let (track_id, clips) = linked_triads(&mut app, ChordFollow::Narrow);
        // note id 3 = G。
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezePitch {
            targets: vec![FreezeTarget { track_id, clip_id: clips[0], note_ids: vec![3] }],
        }));
        assert_eq!(written_and_sounding(&app, track_id, clips[0]), (vec![60, 64, 69], vec![60, 64, 69]));
        assert_eq!(written_and_sounding(&app, track_id, clips[1]), (vec![60, 64, 67], vec![60, 65, 69]));
        let song = app.cur.song_doc.song();
        assert_eq!(song.track_by_id(track_id).expect("track").follow_chords, ChordFollow::Narrow);
    }

    /// 演奏入力のリアルタイム変換: 追従するトラックで弾いた音は playhead の位置のコードへ寄る。コードの試聴が使う
    /// `transposed_preview_key` は寄せない。
    #[test]
    fn played_notes_follow_the_chord_at_the_playhead() {
        let mut app = crate::test_support::headless_app();
        let (track_id, _) = linked_triads(&mut app, ChordFollow::Narrow);
        // playhead 0 = Am: D (62) は同距離の C / E のうち上の E へ。
        assert_eq!(app.preview_key(track_id, 62), Some(64));
        assert_eq!(app.transposed_preview_key(track_id, 62), Some(62));
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::SetEnabled(false)));
        assert_eq!(app.preview_key(track_id, 62), Some(62), "曲全体のスイッチを切ると弾いた音");
    }

    /// 既にあるノートの試聴 (↑↓ で音程を動かしたとき) は playhead ではなく **そのノートの位置のコード** で鳴る
    /// (ピアノロールの薄い枠と同じ鍵盤)。演奏入力 (`preview_key`) は playhead のコードのまま。
    #[test]
    fn nudge_audition_sounds_the_chord_at_the_note_not_at_the_playhead() {
        let mut app = crate::test_support::headless_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.ipc.audio_tx = Some(tx);
        let (track_id, clips) = linked_triads(&mut app, ChordFollow::Narrow);
        app.cur.transport.playhead_beat = Some(0.0); // Am の上
        // F の位置のクリップの E (content の 2 音目) を半音上げて F にする。
        app.set_clip_selection(vec![crate::app::ClipKey { track_id, clip_id: clips[1] }]);
        app.handle_event(AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 1)]));
        app.handle_event(AppEvent::NudgeSelectedNotePitch { octave: false, steps: 1 });
        let on: Vec<u8> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|c| match c {
                common::protocol::AudioCommand::PreviewNoteOn { pitch, .. } => Some(pitch),
                _ => None,
            })
            .collect();
        // F の上の F はコード音 (65)。playhead の Am で寄せると最寄りの E (64) になる。
        assert_eq!(on, vec![65]);
        assert_eq!(app.preview_key(track_id, 65), Some(64), "演奏入力は playhead の Am へ寄る");
    }

    /// グループに追従を設定すると、グループ自身ではなく子孫のトラックに入る (グループはクリップを鳴らさないので
    /// 自分に設定しても子の音は変わらない)。「鳴る音で確定」も子孫へ展開する。
    #[test]
    fn follow_settings_on_a_group_go_to_its_descendants() {
        let mut app = crate::test_support::headless_app();
        // 1 (group) ─┬ 2
        //            └ 3 (group) ─ 4        5 は無関係
        app.edit_song(|song| {
            song.tracks.clear();
            for (id, parent) in [(1, None), (2, Some(1)), (3, Some(1)), (4, Some(3)), (5, None)] {
                song.tracks.push(crate::app::track_with(|t| {
                    t.id = id;
                    t.parent_group_id = parent;
                }));
            }
        });
        let modes = |app: &AppData| -> Vec<ChordFollow> {
            [1, 2, 3, 4, 5].map(|id| app.cur.song_doc.song().track_by_id(id).expect("track").follow_chords).to_vec()
        };
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::SetTracks { track_ids: vec![1], mode: ChordFollow::Narrow }));
        let (off, n) = (ChordFollow::Off, ChordFollow::Narrow);
        assert_eq!(modes(&app), vec![off, n, off, n, off]);
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezeTracks { track_ids: vec![3] }));
        assert_eq!(modes(&app), vec![off, n, off, off, off], "確定もグループ 3 の子 (4) へ");
        // メニューのチェック: グループは子孫が揃っていればそのモード、混ざっていれば付けない。
        assert_eq!(app.track_menu_chord_follow(3), Some(off));
        assert_eq!(app.track_menu_chord_follow(1), None, "2 = Narrow / 4 = Off");
        assert_eq!(app.track_menu_chord_follow(2), Some(n));
        // グループ自身に残っていた値 (グループになる前の設定) は、グループへ設定し直すと消える。
        app.edit_song(|song| song.track_by_id_mut(1).expect("group").follow_chords = ChordFollow::Bass);
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::SetTracks { track_ids: vec![1], mode: off }));
        assert_eq!(modes(&app), vec![off; 5]);
        assert_eq!(app.track_menu_chord_follow(1), Some(off));
    }

    /// ランチャーのセルは曲の位置を持たない (撃った位置のコードで鳴る) ので、鳴る音の枠も確定も出さない。
    /// セルとアレンジのクリップは 1 つの id 空間なので、id だけではアレンジのクリップと区別できない。
    #[test]
    fn a_launcher_cell_shows_written_notes_and_cannot_be_frozen() {
        let mut app = crate::test_support::headless_app();
        let (track_id, clips) = linked_triads(&mut app, ChordFollow::Narrow);
        let cell_id = 950;
        app.edit_song(|song| {
            let scene_id = song.scenes.first().map_or(1, |s| s.id);
            let track = song.track_by_id_mut(track_id).expect("track");
            let mut clip = track.clips[0].clone();
            clip.id = cell_id;
            track.session_clips.push(common::model::SessionClip { scene_id, clip, launch: Default::default() });
        });
        assert!(app.clip_sounding_keys(track_id, clips[0]).expect("clip").is_active());
        assert!(!app.clip_sounding_keys(track_id, cell_id).expect("cell").is_active());
        let before = app.cur.song_doc.undo_depth();
        app.handle_event(AppEvent::ChordFollow(ChordFollowEvent::FreezePitch {
            targets: vec![FreezeTarget { track_id, clip_id: cell_id, note_ids: Vec::new() }],
        }));
        assert_eq!(app.cur.song_doc.undo_depth(), before, "セルは確定しない");
    }
}
