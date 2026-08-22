// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
        if self.selection.selected_clips.len() < 2 {
            self.ui_ephemeral.status_message = format!(
                "Glue: 2 つ以上の clip を選択してください (現在 {} 個)",
                self.selection.selected_clips.len()
            );
            return;
        }

        // Group selected clips by track.
        let mut by_track: std::collections::BTreeMap<u32, Vec<ClipRef>> =
            std::collections::BTreeMap::new();
        for r in self.selected_clip_refs() {
            by_track.entry(r.track).or_default().push(r);
        }

        let mut new_refs: Vec<ClipRef> = Vec::new();
        let mut glued_count = 0usize;
        let mut had_mixed_kind = false;

        for (track_idx, mut refs) in by_track {
            if refs.len() < 2 {
                continue;
            }
            // Sort by start_beat ascending (clip indices may differ).
            refs.sort_by(|a, b| {
                let ta = self
                    .song_doc.song()
                    .tracks
                    .get(a.track as usize)
                    .and_then(|t| t.clips.get(a.clip as usize))
                    .map(|c| c.start_beat)
                    .unwrap_or(f64::INFINITY);
                let tb = self
                    .song_doc.song()
                    .tracks
                    .get(b.track as usize)
                    .and_then(|t| t.clips.get(b.clip as usize))
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
            }
            let mut glue_kind: Option<GlueKind> = None;
            for r in &refs {
                let Some(track) = self.song_doc.song().tracks.get(r.track as usize) else {
                    continue;
                };
                let Some(clip) = track.clips.get(r.clip as usize) else {
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
                        had_mixed_kind = true;
                        break;
                    }
                    // Text clip Glue は後 commit で実装。 まずは「混在」
                    // 扱いで abort、 Image / Video / Audio / MIDI 同士は
                    // 動作維持。
                    ClipContent::Text(_) => {
                        had_mixed_kind = true;
                        break;
                    }
                };
                match glue_kind {
                    None => glue_kind = Some(this_kind),
                    Some(prev) if prev != this_kind => {
                        had_mixed_kind = true;
                        break;
                    }
                    _ => {}
                }
            }
            if had_mixed_kind {
                continue;
            }
            let glue_kind = match glue_kind {
                Some(k) => k,
                None => continue,
            };

            // Compute combined range + collect content fragments.
            let mut combined_start = f64::INFINITY;
            let mut combined_end = f64::NEG_INFINITY;
            let mut combined_name = String::new();
            #[derive(Default)]
            struct Fragments {
                midi_notes: Vec<Note>,
                audio_events: Vec<AudioEvent>,
                video_events: Vec<common::model::VideoEvent>,
                image_events: Vec<common::model::ImageEvent>,
            }
            let mut frags = Fragments::default();

            for r in &refs {
                let Some(track) = self.song_doc.song().tracks.get(r.track as usize) else {
                    continue;
                };
                let Some(clip) = track.clips.get(r.clip as usize) else {
                    continue;
                };
                let s = clip.start_beat;
                let e = s + clip.length_beats;
                if combined_name.is_empty() {
                    combined_name =
                        self.song_doc.song().content_name(clip.content_id).to_string();
                }
                combined_start = combined_start.min(s);
                combined_end = combined_end.max(e);
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
                    // Text clip Glue は後 commit で実装。 abort 済み
                    // (had_mixed_kind = true) なので reach 不能、 防衛的
                    // に no-op。
                    ClipContent::Text(_) => {}
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
                    let track = &song.tracks[track_idx as usize];
                    refs.iter()
                        .map(|r| r.clip as usize)
                        .min()
                        .and_then(|i| track.clips.get(i))
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
                // Remove source clips (descending index to keep earlier
                // indices stable).
                let track = &mut song.tracks[track_idx as usize];
                let mut indices: Vec<usize> =
                    refs.iter().map(|r| r.clip as usize).collect();
                indices.sort_unstable();
                indices.dedup();
                for &idx in indices.iter().rev() {
                    if idx < track.clips.len() {
                        track.clips.remove(idx);
                    }
                }
                // Append the merged clip.
                let new_clip_id = track.alloc_clip_id();
                let new_idx = track.clips.len() as u32;
                track.clips.push(Clip {
                    id: new_clip_id,
                    start_beat: combined_start,
                    length_beats: combined_len,
                    content_id: new_content_id,
                    // 結合 content は combined_start を原点に組み直したので窓は先頭から。
                    content_offset_beats: 0.0,
                    color: None,
                    auto_lipsync: false,
                    lipsync_gen: 0,
                    muted: glue_muted,
                    speaker_id: glue_speaker,
                    singer_name: glue_singer,
                    style_name: glue_style,
                    talk: glue_talk,
                });
                new_refs.push(ClipRef {
                    track: track_idx,
                    clip: new_idx,
                });
                glued_count += 1;
            });
        }

        if had_mixed_kind {
            tracing::warn!("Glue rejected: mixed kinds");
            self.ui_ephemeral.status_message =
                "Glue: MIDI / Audio / Video / Image / Vocal clip が混在しているため Glue できません"
                    .into();
            return;
        }
        if glued_count == 0 {
            tracing::warn!("Glue: glued_count==0 (no track had 2+ clips)");
            self.ui_ephemeral.status_message =
                "Glue: 同じ track 上で 2 つ以上の clip を選択してください".into();
            return;
        }

        tracing::info!(glued_count, ?new_refs, "Glue completed");
        self.select_new_clips(&new_refs);
        self.selection.selected_notes.clear();
        self.ui_ephemeral.status_message = format!("Glue: {glued_count} 箇所を結合しました");
    }

}
