//! handler::glue — 選択 clip の Glue (結合)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{AudioContent, AudioEvent, Clip, ClipContent, MidiContent, Note};

/// r.md #44: audio event を clip の内容窓 `[win_start, win_end)` (content-local 拍) で
/// 切り出す。 窓と交差しなければ `None`。
///
/// Glue は複数 clip を **1 つの新しい content へ焼き込む** 破壊的操作なので、ここでは
/// 「鳴っている範囲」 をそのまま新 event として作り直す必要がある (窓は clip 側に残せない)。
/// 頭を落とす分は現在の frames-per-beat 比で `source_start_frames` を進める線形近似
/// (= split の straddle 処理と同じ規約)。 warp marker / slice を持つ event では近似だが、
/// Glue 自体が content を flat 化する操作なのでここが唯一の妥協点になる。
fn crop_audio_event(ev: &AudioEvent, win_start: f64, win_end: f64) -> Option<AudioEvent> {
    let e0 = ev.event_start_in_clip_beats;
    let e1 = e0 + ev.event_length_beats;
    let c0 = e0.max(win_start);
    let c1 = e1.min(win_end);
    if c1 <= c0 {
        return None;
    }
    let mut out = ev.clone();
    let span = ev.source_end_frames.saturating_sub(ev.source_start_frames);
    // 頭落とし: source を進める (event 長 → source frame の現在比を保つ)。
    if c0 > e0 && ev.event_length_beats > 1e-9 && span > 0 {
        let frac = (c0 - e0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let advance = (span as f64 * frac).max(0.0) as u64;
        out.source_start_frames = ev
            .source_start_frames
            .saturating_add(advance)
            .min(ev.source_end_frames);
        // 頭を落とした分だけ fade in は消費済み。
        out.fade_in_beats = (ev.fade_in_beats - (c0 - e0)).max(0.0);
    }
    // 尻切り: source 窓を新しい長さへ lockstep。
    if c1 < e1 && ev.event_length_beats > 1e-9 && span > 0 {
        let kept = (c1 - c0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let keep_frames = (span as f64 * kept).max(0.0) as u64;
        out.source_end_frames = out
            .source_start_frames
            .saturating_add(keep_frames)
            .min(ev.source_end_frames);
        out.fade_out_beats = (ev.fade_out_beats - (e1 - c1)).max(0.0);
    }
    out.event_start_in_clip_beats = c0;
    out.event_length_beats = c1 - c0;
    Some(out)
}

/// [`crop_audio_event`] の video 版 (source 軸が micro 秒)。
fn crop_video_event(
    ev: &common::model::VideoEvent,
    win_start: f64,
    win_end: f64,
) -> Option<common::model::VideoEvent> {
    let e0 = ev.event_start_in_clip_beats;
    let e1 = e0 + ev.event_length_beats;
    let c0 = e0.max(win_start);
    let c1 = e1.min(win_end);
    if c1 <= c0 {
        return None;
    }
    let mut out = ev.clone();
    let span = ev.source_end_micros.saturating_sub(ev.source_start_micros);
    if c0 > e0 && ev.event_length_beats > 1e-9 && span > 0 {
        let frac = (c0 - e0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let advance = (span as f64 * frac).max(0.0) as u64;
        out.source_start_micros = ev
            .source_start_micros
            .saturating_add(advance)
            .min(ev.source_end_micros);
        out.fade_in_beats = (ev.fade_in_beats - (c0 - e0)).max(0.0);
    }
    if c1 < e1 && ev.event_length_beats > 1e-9 && span > 0 {
        let kept = (c1 - c0) / ev.event_length_beats;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let keep_micros = (span as f64 * kept).max(0.0) as u64;
        out.source_end_micros = out
            .source_start_micros
            .saturating_add(keep_micros)
            .min(ev.source_end_micros);
        out.fade_out_beats = (ev.fade_out_beats - (e1 - c1)).max(0.0);
    }
    out.event_start_in_clip_beats = c0;
    out.event_length_beats = c1 - c0;
    Some(out)
}

impl AppData {
    /// Glue (Consolidate) the currently selected clips into one clip
    /// per track. Mixed-kind selections (MIDI + Audio etc.) are
    /// rejected with a status message. See `docs/plan_audio_clip.md`
    /// §3.3 / §3.3.2.
    pub(crate) fn action_glue_selected_clips(&mut self) {
        let Some(sel) = self.selection.time.clone() else {
            self.ui_ephemeral.status_message = "Glue: 範囲を選択してください".to_string();
            return;
        };
        // `J` は 1 回の操作なので **1 undo step** に束ねる。 境界の分割 (1 回) と
        // トラックごとの結合 (N 回) が別々の step になると、1 回の `J` を戻すのに
        // N+1 回 Undo が要る。
        self.song_doc.begin_gesture();
        // **範囲の境界でクリップを割ってから集める。** はみ出した部分は元のクリップと
        // して残り、範囲の中身だけが 1 クリップへ焼き込まれる (Live の `Ctrl+E`
        // "Split Clip at Selection" と同じ切り出し、`docs/plan_range_selection.md` §7.1)。
        let range_tracks: Vec<u32> = sel
            .lanes
            .iter()
            .filter_map(|l| match l {
                common::model::LaneRef::Track(id) => Some(*id),
                _ => None,
            })
            .collect();
        {
            let (a, b) = (sel.start_beat, sel.end_beat);
            let tracks = range_tracks.clone();
            self.edit_song(|song| {
                for track_id in &tracks {
                    crate::handler::range_ops::split_track_at(song, *track_id, a);
                    crate::handler::range_ops::split_track_at(song, *track_id, b);
                }
            });
        }

        // 範囲に**完全に入る**クリップだけをトラック別に集める (境界で割った後なので
        // 部分的に掛かるクリップはもう無い)。
        let mut by_track: std::collections::BTreeMap<u32, Vec<ClipKey>> =
            std::collections::BTreeMap::new();
        for r in self.selected_clip_refs() {
            let Some(clip) = self.song_doc.song().clip_by_key(r) else {
                continue;
            };
            let (s, e) = clip.song_window();
            if s >= sel.start_beat - 1e-9 && e <= sel.end_beat + 1e-9 {
                by_track.entry(r.track_id).or_default().push(r);
            }
        }

        let mut new_refs: Vec<ClipKey> = Vec::new();
        let mut glued_count = 0usize;
        let mut had_mixed_kind = false;

        for (track_id, mut refs) in by_track {
            if refs.is_empty() {
                continue;
            }
            // 混在フラグは**トラックごとに閉じる**。 ループの外で持つと、1 トラックの
            // 混在で以降の全トラックが skip され、しかも先に結合済みのトラックの編集は
            // 残ったまま「Glue できません」で終わっていた (旧バグ)。
            let mut mixed_in_track = false;
            // Sort by start_beat ascending (clip indices may differ).
            refs.sort_by(|a, b| {
                let ta = self
                    .song_doc.song()
                    .track_by_id(a.track_id)
                    .and_then(|t| t.clip_by_id(a.clip_id))
                    .map(|c| c.start_beat)
                    .unwrap_or(f64::INFINITY);
                let tb = self
                    .song_doc.song()
                    .track_by_id(b.track_id)
                    .and_then(|t| t.clip_by_id(b.clip_id))
                    .map(|c| c.start_beat)
                    .unwrap_or(f64::INFINITY);
                ta.total_cmp(&tb)
            });

            // Detect mixed kinds. Glue is only valid within a single
            // ClipContent variant (= can't merge audio + video). The 3-way
            // enum extends the old `Option<bool>` (= Audio vs MIDI) so
            // Video clips are also eligible for Glue (docs/plan_video.md
            // §4 P6).
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum GlueKind {
                Midi,
                Audio,
                Video,
                Image,
                Text,
            }
            let mut glue_kind: Option<GlueKind> = None;
            for r in &refs {
                let Some(track) = self.song_doc.song().track_by_id(r.track_id) else {
                    continue;
                };
                let Some(clip) = track.clip_by_id(r.clip_id) else {
                    continue;
                };
                let Some(content) = self.song_doc.song().clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                let this_kind = match content {
                    ClipContent::Midi(_) => GlueKind::Midi,
                    ClipContent::Audio(_) => GlueKind::Audio,
                    ClipContent::Video(_) => GlueKind::Video,
                    ClipContent::Image(_) => GlueKind::Image,
                    // Automation clips don't live in `Track.clips` so a
                    // stale link here is unreachable, but be defensive
                    // and treat as a kind change to abort.
                    ClipContent::Automation(_) => {
                        mixed_in_track = true;
                        break;
                    }
                    ClipContent::Text(_) => GlueKind::Text,
                };
                match glue_kind {
                    None => glue_kind = Some(this_kind),
                    Some(prev) if prev != this_kind => {
                        mixed_in_track = true;
                        break;
                    }
                    _ => {}
                }
            }
            if mixed_in_track {
                had_mixed_kind = true;
                continue;
            }
            let glue_kind = match glue_kind {
                Some(k) => k,
                None => continue,
            };

            // **結合範囲 = 選択範囲そのもの** (`docs/plan_range_selection.md` §7.1)。
            // 範囲が中身より広ければ、前後の空白は content 内の「何も無い区間」として
            // 自然に表現される (`content_offset_beats` を負にする必要は無い)。
            let combined_start = sel.start_beat;
            let combined_end = sel.end_beat;
            let mut combined_name = String::new();
            #[derive(Default)]
            struct Fragments {
                midi_notes: Vec<Note>,
                audio_events: Vec<AudioEvent>,
                video_events: Vec<common::model::VideoEvent>,
                image_events: Vec<common::model::ImageEvent>,
                text_events: Vec<common::model::TextEvent>,
            }
            let mut frags = Fragments::default();

            for r in &refs {
                let Some(track) = self.song_doc.song().track_by_id(r.track_id) else {
                    continue;
                };
                let Some(clip) = track.clip_by_id(r.clip_id) else {
                    continue;
                };
                if combined_name.is_empty() {
                    combined_name =
                        self.song_doc.song().content_name(clip.content_id).to_string();
                }
                let Some(content) = self.song_doc.song().clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                // r.md #44: clip は content への「窓」なので、
                // (a) content-local 拍 → combined-local 拍の換算は clip 開始ではなく
                //     **content 原点** (`content_origin_beat`) 基準、
                // (b) 窓の外の note / event は **鳴っていない**ので glue にも含めない。
                // これで左端 trim 済み clip を glue しても、隠れていた中身が復活しない。
                let (win_start, win_end) = clip.content_window();
                let offset_into_combined = clip.content_origin_beat() - combined_start;
                match content {
                    ClipContent::Midi(midi) => {
                        for note in &midi.notes {
                            // sequencer と同じ gate: 発音開始が窓内の note だけ、
                            // 長さは窓末尾で clamp (= 実際に鳴っている姿)。
                            if note.start_beat < win_start || note.start_beat >= win_end {
                                continue;
                            }
                            let dur = note
                                .duration_beats
                                .min(win_end - note.start_beat)
                                .max(0.0);
                            frags.midi_notes.push(Note {
                                start_beat: note.start_beat + offset_into_combined,
                                duration_beats: dur,
                                ..note.clone()
                            });
                        }
                    }
                    ClipContent::Audio(audio) => {
                        for ev in &audio.events {
                            let Some(mut cropped) = crop_audio_event(ev, win_start, win_end)
                            else {
                                continue;
                            };
                            cropped.event_start_in_clip_beats += offset_into_combined;
                            frags.audio_events.push(cropped);
                        }
                    }
                    // Same as the split path above: an Automation
                    // variant referenced from `Track.clips` is a
                    // stale link, skip silently.
                    ClipContent::Automation(_) => {}
                    // Image Glue (`docs/plan_image_overlay.md` §4 P4):
                    // Audio と同じ shift logic。 PiP rect / opacity /
                    // fade / source_id は per-event なので clone してから
                    // event_start を offset するだけ。
                    ClipContent::Image(image) => {
                        for ev in &image.events {
                            // image は時間軸 source を持たないので、窓との交差で
                            // 表示区間を切るだけ。
                            let e0 = ev.event_start_in_clip_beats.max(win_start);
                            let e1 = (ev.event_start_in_clip_beats
                                + ev.event_length_beats)
                                .min(win_end);
                            if e1 <= e0 {
                                continue;
                            }
                            frags.image_events.push(common::model::ImageEvent {
                                event_start_in_clip_beats: e0 + offset_into_combined,
                                event_length_beats: e1 - e0,
                                ..ev.clone()
                            });
                        }
                    }
                    // Video Glue (docs/plan_video.md §4 P6): same shift
                    // logic as Audio. source_micros range stays as-is
                    // per event since Glue doesn't change content
                    // mapping, only repositions the events on the
                    // combined timeline.
                    ClipContent::Video(video) => {
                        for ev in &video.events {
                            let Some(mut cropped) = crop_video_event(ev, win_start, win_end)
                            else {
                                continue;
                            };
                            cropped.event_start_in_clip_beats += offset_into_combined;
                            frags.video_events.push(cropped);
                        }
                    }
                    // Text (字幕 / タイトル) は時間軸 source を持たないので、
                    // image と同じく窓との交差で表示区間を切るだけ。
                    ClipContent::Text(text) => {
                        for ev in &text.events {
                            let e0 = ev.event_start_in_clip_beats.max(win_start);
                            let e1 = (ev.event_start_in_clip_beats + ev.event_length_beats)
                                .min(win_end);
                            if e1 <= e0 {
                                continue;
                            }
                            frags.text_events.push(common::model::TextEvent {
                                event_start_in_clip_beats: e0 + offset_into_combined,
                                event_length_beats: e1 - e0,
                                ..ev.clone()
                            });
                        }
                    }
                }
            }
            if !combined_start.is_finite() || !combined_end.is_finite() {
                continue;
            }

            // Re-walk to fix offsets now that we know combined_start.
            // (The first pass used a tentative `combined_start` that
            // updated as we iterated; re-shift everything by the
            // delta between the first clip's start and the actual
            // combined_start. In sorted order they should already
            // match since clips are sorted by start_beat and
            // combined_start = first clip's start, so the no-op case
            // is the common one — but be defensive.)

            let combined_len = combined_end - combined_start;
            let new_content = match glue_kind {
                GlueKind::Audio => {
                    // v29: 複数 content 由来の event id は衝突し得るので、
                    // merged content では振り直す (per-content unique が不変条件)。
                    let mut events = frags.audio_events;
                    for (i, e) in events.iter_mut().enumerate() {
                        e.id = i as u32 + 1;
                    }
                    ClipContent::Audio(AudioContent {
                        next_event_id: events.len() as u32 + 1,
                        events,
                    })
                }
                GlueKind::Video => ClipContent::Video(common::model::VideoContent {
                    events: frags.video_events,
                }),
                GlueKind::Image => ClipContent::Image(common::model::ImageContent {
                    events: frags.image_events,
                }),
                GlueKind::Text => ClipContent::Text(common::model::TextContent {
                    events: frags.text_events,
                }),
                GlueKind::Midi => {
                    let mut notes = frags.midi_notes;
                    notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                    // glue で別 clip 由来の同一ピッチ note が時間的に
                    // 重なり得るので、全 note を勝者として重なりを解消する。
                    let all: Vec<u32> = (0..notes.len() as u32).collect();
                    resolve_note_overlaps(&mut notes, &all);
                    // v29: merged content では note id を振り直す (上の Audio と同理)。
                    for (i, n) in notes.iter_mut().enumerate() {
                        n.id = i as u32 + 1;
                    }
                    ClipContent::Midi(MidiContent {
                        next_note_id: notes.len() as u32 + 1,
                        notes,
                    })
                }
            };
            self.edit_song(|song| {
                let new_content_id = song.alloc_content_id();
                song.clip_contents.insert(new_content_id, new_content);
                // merged clip の名前は content_id 単位 SSoT へ。
                if !combined_name.is_empty() {
                    song.set_content_name(new_content_id, combined_name.clone());
                }

                // merged clip は最初 (= 最も早い index = sorted 先頭) の
                // source clip の声を採用 (複数声混在時のポリシー)。 source 削除前に capture。
                // merged clip の mute も代表 (最早) source clip の値を採用 (声と同ポリシー)。
                let (glue_speaker, glue_singer, glue_style, glue_talk, glue_muted) = {
                    // `refs` は上で start_beat 昇順に並べ替え済みなので先頭が代表。
                    refs.first()
                        .and_then(|r| song.clip_by_key(*r))
                        .map(|c| {
                            (
                                c.speaker_id,
                                c.singer_name.clone(),
                                c.style_name.clone(),
                                c.talk,
                                c.muted,
                            )
                        })
                        .unwrap_or((0, String::new(), String::new(), None, false))
                };
                // 元クリップを消す。住所が id なので「後ろから消して index の
                // 詰まりを避ける」儀式は要らない。
                let Some(track) = song.track_by_id_mut(track_id) else {
                    return;
                };
                for r in &refs {
                    track.remove_clip_by_id(r.clip_id);
                }
                // Append the merged clip.
                let new_clip_id = track.place_clip(Clip {
                    id: 0,
                    start_beat: combined_start,
                    length_beats: combined_len,
                    content_id: new_content_id,
                    // 結合 content は combined_start を原点に組み直したので窓は先頭から。
                    content_offset_beats: 0.0,
                    // 新規クリップにクロスフェードの張り出しは無い。
                    xfade_lead_beats: 0.0,
                    xfade_tail_beats: 0.0,
                    color: None,
                    auto_lipsync: false,
                    lipsync_gen: 0,
                    muted: glue_muted,
                    speaker_id: glue_speaker,
                    singer_name: glue_singer,
                    style_name: glue_style,
                    talk: glue_talk,
                });
                new_refs.push(ClipKey { track_id, clip_id: new_clip_id });
                glued_count += 1;
            });
        }

        if had_mixed_kind {
            tracing::warn!("Glue rejected: mixed kinds");
            // 混在したトラックだけを飛ばし、他のトラックの結合は活かす
            // (旧実装は 1 トラックの混在で以降の全トラックを skip しつつ、
            // 先に結合済みのトラックの編集は残したまま中断していた)。
            if glued_count == 0 {
                self.ui_ephemeral.status_message =
                    "Glue: 種類が混在しているため結合できません".into();
                return;
            }
        }
        if glued_count == 0 {
            self.song_doc.end_gesture();
            tracing::warn!("Glue: glued_count==0 (範囲内にクリップが無い)");
            self.ui_ephemeral.status_message =
                "Glue: 範囲の中にクリップがありません".into();
            return;
        }

        self.song_doc.end_gesture();
        tracing::info!(glued_count, ?new_refs, "Glue completed");
        self.select_new_clips(&new_refs);
        self.ui_ephemeral.status_message = if had_mixed_kind {
            format!("Glue: {glued_count} 箇所を結合しました (種類が混在したトラックは除外)")
        } else {
            format!("Glue: {glued_count} 箇所を結合しました")
        };
    }

}
