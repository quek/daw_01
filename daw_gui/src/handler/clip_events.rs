//! handler::clip_events — clip 内 audio/image/text event の field 編集 + font picker
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;

impl AppData {
    /// `target` clip の first event の `reversed` 値を読む。 audio で
    /// ない / event が空 / 範囲外なら `false`。 メニューの toggle 用。
    pub(crate) fn is_clip_audio_event_reversed(&self, target: ClipRef) -> bool {
        self.song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| {
                if let Some(common::model::ClipContent::Audio(audio)) =
                    self.song_doc.song().clip_contents.get(&c.content_id)
                {
                    audio.events.first().map(|e| e.reversed)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    /// `AudioEvent.reversed` を更新 (`docs/plan_audio_clip.md` §3.8)。
    /// audio_editor で event を選択中なら当該 event のみ、 さもなくば
    /// 全 event に broadcast (= multi-event 対応 / 1 clip 1 event 互換、
    /// PR-D 段階 2)。
    pub(crate) fn set_clip_audio_event_reversed(&mut self, target: ClipRef, reversed: bool) {
        self.mutate_audio_events_in_clip(target, |e| e.reversed = reversed);
    }

    /// `targets` の clip が **全て** muted なら `true` (空なら `false`)。`q` の
    /// toggle 方向決定用 (全 muted → unmute、 1 つでも非 muted → 全 mute)。
    pub fn all_clips_muted(&self, targets: &[ClipRef]) -> bool {
        !targets.is_empty()
            && targets.iter().all(|t| {
                self.song_doc.song()
                    .tracks
                    .get(t.track as usize)
                    .and_then(|tr| tr.clips.get(t.clip as usize))
                    .is_some_and(|c| c.muted)
            })
    }

    /// clip 内 `notes` (index) が **全て** muted なら `true` (空 / 非 MIDI は `false`)。
    /// `q` の note mute toggle 方向決定用。
    pub fn all_notes_muted(&self, notes: &[u32]) -> bool {
        // `notes` は packed note id。各 id を所属クリップへ decode し、 そのクリップの
        // 当該 note が muted か見る (toggle 方向 = 「全部 muted なら unmute」 を複数クリップ跨ぎで判定)。
        if notes.is_empty() {
            return false;
        }
        let shown = self.shown_pianoroll_clips();
        notes.iter().all(|&id| {
            let Some((r, local)) = Self::decode_note_id_in(&shown, id) else {
                return false;
            };
            self.song_doc.song()
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .and_then(|c| self.song_doc.song().clip_notes(c).get(local).map(|n| n.muted))
                .unwrap_or(false)
        })
    }

    /// clip-level mute (`Clip.muted`) を設定する。MIDI / audio / video / image /
    /// 字幕 / 歌唱すべての content type 共通の単一 SSoT。`q` ショートカット (`SetClipsMuted`)、
    /// 各 inspector の "Mute" トグル (`DiscreteClipEdit::Muted` / `TextMuted`)、単発の
    /// `SetClipMuted` / `SetClipTextMuted` event がすべてここを経由する。変更があれば
    /// `flush_song_sync` で daw_audio へ LoadSong flush し、再生・書き出しに反映する
    /// (is_dirty もそこで立つ)。
    pub(crate) fn set_clip_muted(&mut self, target: ClipRef, muted: bool) {
        self.edit_song_checked(|song| {
            if let Some(track) = song.tracks.get_mut(target.track as usize)
                && let Some(clip) = track.clips.get_mut(target.clip as usize)
                && clip.muted != muted
            {
                clip.muted = muted;
                true
            } else {
                false
            }
        });
    }

    /// clip の `ClipContent::Midi` 内 note (index 指定) の `Note.muted` を一括設定する。
    /// `selected_notes` と同じ index 空間。linked clip は content (= notes) を共有するので
    /// mute も linked clip 間で共有される。変更は edit_song の epoch bump を runner の
    /// frame flush が host へ LoadSong する (sequencer が muted note を skip して再生・
    /// 書き出しから除外)。
    pub(crate) fn set_notes_muted(&mut self, notes: &[u32], muted: bool) {
        // `notes` は packed note id。所属クリップごとに分配し、各クリップの
        // 当該 note の mute を設定する (locked クリップは for_each_note_clip_group が除外)。
        self.for_each_note_clip_group(
            notes.iter().map(|&id| (id, ())),
            |app, _slot, r, items| {
                app.edit_song_checked(|song| {
                    let Some(clip_notes) =
                        song.notes_in_clip_mut(r.track as usize, r.clip as usize)
                    else {
                        return false;
                    };
                    let mut c = false;
                    for &(local, ()) in items {
                        if let Some(n) = clip_notes.get_mut(local)
                            && n.muted != muted
                        {
                            n.muted = muted;
                            c = true;
                        }
                    }
                    c
                });
            },
        );
    }

    /// `AudioEvent.stretch_mode` を更新。 `compile_audio_schedule` が
    /// 次の LoadSong で再 compile し、 Repitch の場合は pitch_ratio の
    /// 再計算が走る。 Phase 1 で再生に効くのは Raw / Repitch のみ。
    pub(crate) fn set_clip_audio_event_stretch_mode(
        &mut self,
        target: ClipRef,
        mode: common::model::StretchMode,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.stretch_mode = mode);
        // B1 (r.md #8): Slice へ切替時、 onsets 未検出の event に transient 検出を
        // 走らせ slice の trigger 位置を埋める (検出済 / 非 Slice は何もしない)。
        if mode == common::model::StretchMode::Slice {
            self.detect_onsets_for_clip(target);
        }
    }

    /// B12 (r.md #8): 選択 audio clip の transient を検出し beat grid (16th) に snap
    /// した warp markers を生成する (auto-warp、 Ableton 流)。 onset は B1 と同じ
    /// `detect_onsets`、 grid 整列は純関数 `warp_markers_from_onsets`。 warp が効くよう
    /// 該当 event を `Stretch` mode に切替える。 transient が無い event は markers 空
    /// (= uniform stretch のまま)。 OFF-RT。 buffer 未 decode の event は skip。
    pub(crate) fn auto_warp_clip(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let n_events = match self.song_doc.song().clip_contents.get(&content_id) {
            Some(common::model::ClipContent::Audio(a)) => a.events.len(),
            _ => return,
        };
        let indices = self.audio_event_target_indices(target, n_events);

        // Phase A: 対象 event の source range + 配置 beat 長 (immutable borrow)。
        let mut jobs: Vec<(usize, common::model::AudioSourceId, u64, u64, f64)> = Vec::new();
        if let Some(common::model::ClipContent::Audio(a)) =
            self.song_doc.song().clip_contents.get(&content_id)
        {
            for &i in &indices {
                if let Some(e) = a.events.get(i) {
                    jobs.push((
                        i,
                        e.source_id,
                        e.source_start_frames,
                        e.source_end_frames,
                        e.event_length_beats,
                    ));
                }
            }
        }
        if jobs.is_empty() {
            return;
        }

        // Phase B: onset 検出 → grid snap warp markers (OFF-RT)。
        let mut results: Vec<(usize, Vec<common::model::BeatMarker>)> = Vec::new();
        for (i, source_id, start, end, length_beats) in jobs {
            let Some(buf) = self.media.audio_source_cache.get(source_id) else {
                continue;
            };
            let s = start.min(buf.frames) as usize;
            let e = end.min(buf.frames) as usize;
            let source_len = end.saturating_sub(start);
            let mono = buf.downmix_mono(s, e);
            if mono.is_empty() || source_len == 0 || length_beats <= 0.0 {
                continue;
            }
            let onsets = common::onset::detect_onsets(&mono, buf.sample_rate, 0.5);
            let markers = common::audio_render::warp_markers_from_onsets(
                &onsets,
                start,
                source_len,
                length_beats,
                4,
            );
            // anchor 2 件のみ = transient 無し → 空で uniform stretch を維持。
            results.push((i, if markers.len() > 2 { markers } else { Vec::new() }));
        }

        // Phase C: 書き戻し (warp 有効 event は Stretch mode へ) + engine 再 sync。
        let (warped, changed) = self
            .edit_song(move |song| {
                let mut warped = 0usize;
                let mut changed = false;
                if let Some(common::model::ClipContent::Audio(a)) =
                    song.clip_contents.get_mut(&content_id)
                {
                    for (i, markers) in results {
                        if let Some(ev) = a.events.get_mut(i) {
                            if !markers.is_empty() {
                                ev.stretch_mode = common::model::StretchMode::Stretch;
                                warped += 1;
                            }
                            ev.beat_markers = markers;
                            changed = true;
                        }
                    }
                }
                (warped, changed)
            })
            .unwrap_or((0, false));
        if changed {
            self.ui_ephemeral.status_message = format!("Auto-Warp: {warped} event を beat grid に整列");
        }
    }

    /// B1 (r.md #8): Slice 切替時に GUI decoded buffer から transient を検出して
    /// `AudioEvent.onsets` (= slice trigger 位置、 `source_start_frames` 起点 0
    /// base、 `slice_sample_at` の contract に一致) を埋める。 既に onsets を持つ
    /// event は前回検出 / 将来の user 編集を尊重して skip。 OFF-RT (buffer を 1 回
    /// scan)。 buffer 未 decode の event は skip (= 空 onsets で Raw 等価のまま)。
    pub(crate) fn detect_onsets_for_clip(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let n_events = match self.song_doc.song().clip_contents.get(&content_id) {
            Some(common::model::ClipContent::Audio(a)) => a.events.len(),
            _ => return,
        };
        let indices = self.audio_event_target_indices(target, n_events);

        // Phase A: 検出対象 (onsets 空) の event index + source range を集める
        // (immutable borrow)。
        let mut jobs: Vec<(usize, common::model::AudioSourceId, u64, u64)> = Vec::new();
        if let Some(common::model::ClipContent::Audio(a)) =
            self.song_doc.song().clip_contents.get(&content_id)
        {
            for &i in &indices {
                if let Some(e) = a.events.get(i)
                    && e.onsets.is_empty()
                {
                    jobs.push((i, e.source_id, e.source_start_frames, e.source_end_frames));
                }
            }
        }
        if jobs.is_empty() {
            return;
        }

        // Phase B: decoded buffer を mono downmix して OFF-RT 検出。
        let mut results: Vec<(usize, Vec<u64>)> = Vec::new();
        for (i, source_id, start, end) in jobs {
            let Some(buf) = self.media.audio_source_cache.get(source_id) else {
                continue;
            };
            let start = start.min(buf.frames) as usize;
            let end = end.min(buf.frames) as usize;
            let mono = buf.downmix_mono(start, end);
            if mono.is_empty() {
                continue;
            }
            let onsets = common::onset::detect_onsets(&mono, buf.sample_rate, 0.5);
            results.push((i, onsets));
        }

        // Phase C: onsets を書き戻し audio engine へ再 sync (mutable borrow)。
        let _ = self
            .edit_song(move |song| {
                let mut changed = false;
                if let Some(common::model::ClipContent::Audio(a)) =
                    song.clip_contents.get_mut(&content_id)
                {
                    for (i, onsets) in results {
                        if let Some(e) = a.events.get_mut(i) {
                            e.onsets = onsets;
                            changed = true;
                        }
                    }
                }
                changed
            })
            .unwrap_or(false);
    }

    pub(crate) fn set_clip_audio_event_gain_db(&mut self, target: ClipRef, gain_db: f32) {
        let gain_db = gain_db.clamp(-80.0, 24.0);
        self.mutate_audio_events_in_clip(target, |e| e.gain_db = gain_db);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_pan(&mut self, target: ClipRef, pan: f32) {
        let pan = pan.clamp(-1.0, 1.0);
        self.mutate_audio_events_in_clip(target, |e| e.pan = pan);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_pitch_semitones(&mut self, target: ClipRef, semitones: f32) {
        // Bitwig spec §3.6: Pitch range is -96 .. +96 semitones.
        let semitones = semitones.clamp(-96.0, 96.0);
        self.mutate_audio_events_in_clip(target, |e| e.pitch_semitones = semitones);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    /// audio inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current audio
    /// clip に同期する純 marker (= 多数の audio 編集パス / song 差し替えから
    /// 呼ばれる)。 target が audio clip を解決できなければ `None` 化する。
    pub(crate) fn resync_clip_audio_event_edit_buffers(&mut self, target: ClipRef) {
        let resolved = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song_doc.song().clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Audio(_)));
        self.ui_ephemeral.clip_edit_buffer_target = if resolved { Some(target) } else { None };
    }


    /// r.md #38: clip 内の **1 event** の fade を content 種別に依らず書き換える。
    ///
    /// アレンジ画面の fade 角 drag はこれを使う。 audio / video / image / text の
    /// 4 種は同じ fade フィールドを持ち、 適用側も同じ curve 式を通るので、
    /// 種別ごとの setter を 4 本用意する必要はない
    /// (`ClipContent::set_event_fade` が唯一の書き込み口)。
    ///
    /// clamp は caller (`f`) の責務。 `EventFade::len_beats` が上限。
    pub(crate) fn set_clip_event_fade(
        &mut self,
        target: crate::app_types::ClipEventRef,
        f: impl FnOnce(common::model::EventFade) -> common::model::EventFade,
    ) {
        let Some(content_id) = self
            .song_doc
            .song()
            .tracks
            .get(target.clip.track as usize)
            .and_then(|t| t.clips.get(target.clip.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let index = target.event as usize;
        self.edit_song(|song| {
            song.clip_contents
                .get_mut(&content_id)
                .is_some_and(|c| c.set_event_fade(index, f))
        });
        // audio の inspector edit buffer はこの clip の値を映すので resync する
        // (他 content 種別の setter は自前の resync を持つが、 fade は arrangement 側
        // からしか来ないので audio 側だけで十分)。
        self.resync_clip_audio_event_edit_buffers(target.clip);
    }

    pub(crate) fn set_clip_audio_event_fade_in_beats(&mut self, target: ClipRef, beats: f64) {
        // r.md #38: 上限は **event 長**。 音 (`audio_clip_renderer`) は event 長基準で
        // fade を掛けるので、 clip 長で clamp すると clip より短い event
        // (trim / split 後) で fade がフルゲインに到達せず絵と音がずれる。
        self.mutate_audio_events_in_clip(target, |e| {
            e.fade_in_beats = beats.clamp(0.0, e.event_length_beats.max(0.0));
        });
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_fade_out_beats(&mut self, target: ClipRef, beats: f64) {
        self.mutate_audio_events_in_clip(target, |e| {
            e.fade_out_beats = beats.clamp(0.0, e.event_length_beats.max(0.0));
        });
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_audio_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    // -------- Image event editors (`docs/plan_image_overlay.md` §4 P4) ----

    pub(crate) fn set_clip_image_event_x(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.x = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_y(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.y = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_w(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.w = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_h(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.h = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_opacity(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.opacity = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_rotation_radians(&mut self, target: ClipRef, value: f32) {
        // -π..=π で wrap して保存。 lane override 経由でも同じ wrap が
        // composite で適用される (= preview 表示は modulo 2π)。
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_image_events_in_clip(target, |e| e.rotation_radians = wrapped);
        self.resync_clip_image_event_edit_buffers(target);
    }

    /// docs/plan_text_overlay.md §4 P6: image と同 idiom の text event
    /// setter 群。 drag / inspector commit / lane override 経由のいずれも
    /// このパスで TextEvent.field を直接書く。
    pub(crate) fn mutate_text_events_in_clip<F>(&mut self, target: ClipRef, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::TextEvent),
    {
        let Some(content_id) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return false;
        };
        self.edit_song_checked(move |song| {
            if let Some(common::model::ClipContent::Text(t)) =
                song.clip_contents.get_mut(&content_id)
            {
                if t.events.is_empty() {
                    return false;
                }
                for event in &mut t.events {
                    f(event);
                }
                true
            } else {
                false
            }
        })
    }

    pub(crate) fn set_clip_text_event_x(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.x = value);
    }

    pub(crate) fn set_clip_text_event_y(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.y = value);
    }

    pub(crate) fn set_clip_text_event_w(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.w = value);
    }

    pub(crate) fn set_clip_text_event_h(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.h = value);
    }

    pub(crate) fn set_clip_text_event_rotation_radians(&mut self, target: ClipRef, value: f32) {
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_text_events_in_clip(target, |e| e.rotation_radians = wrapped);
    }

    pub(crate) fn set_clip_text_event_content(&mut self, target: ClipRef, value: String) {
        // 単一行 text のみ (`plan_text_overlay.md` §1.1)、 '\n' は除外。
        let value = value.replace(['\n', '\r'], " ");
        if self.mutate_text_events_in_clip(target, |e| e.text = value.clone()) {
            // (talk) Text は VOICEVOX トラックでは読み上げ原稿。本文変更を builtin へ
            // 再 flush (= 新テキストで talk 再合成) + 口パク再生成。非 VOICEVOX
            // トラックの Text 編集では sync_vocal_metadata は no-op、debounce も
            // bound track 無しで無害。
            self.sync_vocal_metadata();
            self.mark_lipsync_dirty();
        }
        self.resync_clip_text_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_text_event_font_family(&mut self, target: ClipRef, value: String) {
        self.mutate_text_events_in_clip(target, |e| e.font_family = value.clone());
        self.resync_clip_text_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_text_event_align(&mut self, target: ClipRef, value: common::model::TextAlign) {
        self.mutate_text_events_in_clip(target, |e| e.align = value);
    }

    pub(crate) fn set_clip_text_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_text_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_text_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_text_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    /// docs/plan_text_overlay.md §4 P5: 23 numeric field + 2 fade beats
    /// を 1 関数で dispatch。 各 field の clamp / wrap rule を inline 適用。
    /// X/Y/W/H/Rotation は P6 drag 経路の setter を流用して double-define
    /// を回避。
    pub(crate) fn set_clip_text_num_field(
        &mut self,
        target: ClipRef,
        field: TextNumField,
        value: f32,
    ) {
        use TextNumField as F;
        match field {
            F::X => self.set_clip_text_event_x(target, value),
            F::Y => self.set_clip_text_event_y(target, value),
            F::W => self.set_clip_text_event_w(target, value),
            F::H => self.set_clip_text_event_h(target, value),
            F::Rotation => self.set_clip_text_event_rotation_radians(target, value),
            F::FontSize => {
                let v = value.max(1.0);
                self.mutate_text_events_in_clip(target, |e| e.font_size_px = v);
            }
            F::Opacity => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.opacity = v);
            }
            F::FillR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[0] = v);
            }
            F::FillG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[1] = v);
            }
            F::FillB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[2] = v);
            }
            F::FillA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[3] = v);
            }
            F::OutlineR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[0] = v);
            }
            F::OutlineG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[1] = v);
            }
            F::OutlineB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[2] = v);
            }
            F::OutlineA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[3] = v);
            }
            F::OutlineWidth => {
                let v = value.max(0.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_width_px = v);
            }
            F::ShadowR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[0] = v);
            }
            F::ShadowG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[1] = v);
            }
            F::ShadowB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[2] = v);
            }
            F::ShadowA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[3] = v);
            }
            F::ShadowOffsetX => {
                self.mutate_text_events_in_clip(target, |e| e.shadow_offset_px.0 = value);
            }
            F::ShadowOffsetY => {
                self.mutate_text_events_in_clip(target, |e| e.shadow_offset_px.1 = value);
            }
            F::ShadowBlur => {
                let v = value.max(0.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_blur_px = v);
            }
            F::FadeInBeats => {
                // r.md #38: text_compose も event 長基準で fade を適用するので上限は event 長。
                let v = f64::from(value);
                self.mutate_text_events_in_clip(target, |e| {
                    e.fade_in_beats = v.clamp(0.0, e.event_length_beats.max(0.0));
                });
            }
            F::FadeOutBeats => {
                let v = f64::from(value);
                self.mutate_text_events_in_clip(target, |e| {
                    e.fade_out_beats = v.clamp(0.0, e.event_length_beats.max(0.0));
                });
            }
        }
        self.resync_clip_text_event_edit_buffers(target);
    }

    pub(crate) fn commit_clip_text_content_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.ui_ephemeral.clip_text_content_edit_text.clone();
        self.set_clip_text_event_content(target, value);
    }

    pub(crate) fn commit_clip_text_font_family_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.ui_ephemeral.clip_text_font_family_edit_text.clone();
        self.set_clip_text_event_font_family(target, value);
    }

    // -------- Font picker -------------------------------------

    /// 編集対象 text クリップの現在のフォント名 (先頭 event)。text クリップで
    /// なければ `None`。
    pub(crate) fn clip_text_font_family(&self, target: ClipRef) -> Option<String> {
        self.song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song_doc.song().clip_contents.get(&c.content_id))
            .and_then(|content| content.text_events())
            .and_then(|events| events.first())
            .map(|e| e.font_family.clone())
    }

    pub(crate) fn open_font_picker(&mut self) {
        // anchor が text クリップのときだけ開く (Font ボタンは text inspector に
        // しか出ないが防衛的に確認)。
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let Some(original) = self.clip_text_font_family(target) else {
            return;
        };
        self.ui_ephemeral.font_picker_target = Some(target);
        self.ui_ephemeral.font_picker_restore = original;
        self.ui_ephemeral.font_picker_query.clear();
        self.ui_ephemeral.font_picker_cursor = 0;
        self.ui_ephemeral.is_font_picker_open = true;
        // ピッカー session 全体 (プレビュー hover/arrow 群 + commit) を 1 gesture に
        // bracket する。 これで最初のプレビューが「元フォント」を snapshot し、 以後の
        // プレビュー/commit は squash されて **1 undo で元に戻る**。 bracket が無いと
        // hover ごとに fresh gesture id → プレビュー 1 回ごとに undo step が積まれ、
        // commit も元を復元しない (M3)。 commit / cancel で end_gesture する。
        self.song_doc.begin_gesture();
        self.refresh_font_picker_visible();
        // システムフォント列挙は重い (~20-860ms) ので background で 1 度だけ。
        if self.ui_ephemeral.font_picker_families.is_empty() && !self.ui_ephemeral.font_picker_loading {
            self.begin_font_load();
        }
    }

    pub(crate) fn begin_font_load(&mut self) {
        self.ui_ephemeral.font_picker_loading = true;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            let families = daw_ui_core::available_font_families();
            proxy.send(AppEvent::FontFamiliesLoaded(families));
        });
    }

    pub(crate) fn on_font_families_loaded(&mut self, families: Vec<String>) {
        self.ui_ephemeral.font_picker_families = families;
        self.ui_ephemeral.font_picker_loading = false;
        self.refresh_font_picker_visible();
    }

    pub(crate) fn refresh_font_picker_visible(&mut self) {
        let query = self.ui_ephemeral.font_picker_query.trim();
        let mut visible: Vec<String> = Vec::new();
        // query が空のときだけ先頭に「デフォルト」行 (`""`) を出す。
        if query.is_empty() {
            visible.push(String::new());
            visible.extend(self.ui_ephemeral.font_picker_families.iter().cloned());
        } else {
            visible.extend(
                self.ui_ephemeral.font_picker_families
                    .iter()
                    .filter(|f| crate::fuzzy::subsequence_match(f, query))
                    .cloned(),
            );
        }
        self.ui_ephemeral.font_picker_visible = visible;
        self.ui_ephemeral.font_picker_cursor = 0;
    }

    pub(crate) fn move_font_picker_cursor(&mut self, delta: i32) {
        let len = self.ui_ephemeral.font_picker_visible.len();
        if len == 0 {
            return;
        }
        self.ui_ephemeral.font_picker_cursor =
            (self.ui_ephemeral.font_picker_cursor as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.preview_font_at_cursor();
    }

    pub(crate) fn hover_font_in_picker(&mut self, idx: usize) {
        // 既に cursor がそこなら no-op (= hover 中の毎フレーム連発を抑止)。
        if idx >= self.ui_ephemeral.font_picker_visible.len() || self.ui_ephemeral.font_picker_cursor == idx {
            return;
        }
        self.ui_ephemeral.font_picker_cursor = idx;
        self.preview_font_at_cursor();
    }

    /// cursor 位置のフォントを編集対象クリップへライブ適用する。 ピッカー
    /// session の gesture (open_font_picker が begin) 内なので、 プレビュー群 +
    /// commit は 1 undo step に squash され、 最初のプレビューが元フォントを
    /// snapshot する (`""` = renderer default)。 song を書き換えるため dirty には
    /// なる (epoch ベース dirty の性質)。
    pub(crate) fn preview_font_at_cursor(&mut self) {
        let Some(target) = self.ui_ephemeral.font_picker_target else {
            return;
        };
        let Some(family) = self.ui_ephemeral.font_picker_visible.get(self.ui_ephemeral.font_picker_cursor).cloned() else {
            return;
        };
        self.set_clip_text_event_font_family(target, family);
    }

    pub(crate) fn commit_font_from_picker(&mut self, family: String) {
        let Some(target) = self.ui_ephemeral.font_picker_target else {
            return;
        };
        // 元 → 選択 を 1 undo step にするため、 一旦元へ戻してから snapshot し
        // (= undo 先 = 元フォント)、 選択フォントを適用する (preview で既に選択
        // 値になっていても結果は同じ)。
        self.set_clip_text_event_font_family(target, self.ui_ephemeral.font_picker_restore.clone());
        self.set_clip_text_event_font_family(target, family);
        // commit 経路では close_font_picker (on_close) の restore を no-op 化する
        // ため target を先に落とす。
        self.ui_ephemeral.font_picker_target = None;
        self.ui_ephemeral.is_font_picker_open = false;
        // session gesture を閉じる (open_font_picker の begin_gesture と対)。
        self.song_doc.end_gesture();
    }

    pub(crate) fn close_font_picker(&mut self) {
        // cancel: preview で変えた font を元へ戻す。commit 済みなら target は
        // None なので no-op。
        if let Some(target) = self.ui_ephemeral.font_picker_target {
            self.set_clip_text_event_font_family(target, self.ui_ephemeral.font_picker_restore.clone());
        }
        self.ui_ephemeral.is_font_picker_open = false;
        self.ui_ephemeral.font_picker_target = None;
        // session gesture を閉じる (open_font_picker の begin_gesture と対)。
        // commit 済み (target 既に None) でも呼ぶ: begin/end を必ず対にする。
        self.song_doc.end_gesture();
    }

    /// docs/plan_text_overlay.md §4 P5: clip 切替 / Undo / Redo / lane
    /// override 変化等で文字列 edit buffer (content / font_family) を current
    /// TextEvent の値で再構築。 25 numeric field は scrubable_number
    /// 化され現値を summary から直接読むため、 数値 buffer の再生成は不要に
    /// なった。 target が Text variant でないなら文字列 buffer を空にして
    /// `clip_edit_buffer_target` を `None`。
    pub(crate) fn resync_clip_text_event_edit_buffers(&mut self, target: ClipRef) {
        let event_snapshot = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song_doc.song().clip_contents.get(&c.content_id))
            .and_then(|content| content.text_events())
            .and_then(|events| events.first())
            .cloned();
        let Some(ev) = event_snapshot else {
            self.ui_ephemeral.clip_text_content_edit_text.clear();
            self.ui_ephemeral.clip_text_font_family_edit_text.clear();
            self.ui_ephemeral.clip_edit_buffer_target = None;
            return;
        };
        self.ui_ephemeral.clip_text_content_edit_text = ev.text.clone();
        self.ui_ephemeral.clip_text_font_family_edit_text = ev.font_family.clone();
        self.ui_ephemeral.clip_edit_buffer_target = Some(target);
    }

    /// docs/plan_text_overlay.md §4 P5: text inspector が表示する
    /// snapshot (= image idiom)。 selected_clip が Text variant の clip
    /// を指していて、 first event があれば `Some` を返す。 各 numeric
    /// field の `*_automated` は対応する TextBuiltin lane が track に
    /// 存在するか。
    pub fn inspector_text_event_summary(&self) -> Option<InspectorTextEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.song_doc.song().tracks.get(cref.track as usize)?;
        let clip = track.clips.get(cref.clip as usize)?;
        let common::model::ClipContent::Text(t) =
            self.song_doc.song().clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let event = t.events.first()?;
        let mut automated = std::collections::HashSet::new();
        for lane in &track.automation_lanes {
            if let common::model::AutomationTarget::TextBuiltin(p) = lane.target {
                automated.insert(p);
            }
        }
        Some(InspectorTextEventSummary {
            target: cref,
            // "Mute" トグル状態は clip-level `Clip.muted` を表示する (SSoT)。
            muted: clip.muted,
            align: event.align,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
            automated,
            fade_max_beats: event.event_length_beats,
            event: event.clone(),
        })
    }

    pub(crate) fn set_clip_image_event_fade_in_beats(&mut self, target: ClipRef, beats: f64) {
        // r.md #38: image_compose も event 長基準で fade を適用するので上限は event 長。
        self.mutate_image_events_in_clip(target, |e| {
            e.fade_in_beats = beats.clamp(0.0, e.event_length_beats.max(0.0));
        });
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_fade_out_beats(&mut self, target: ClipRef, beats: f64) {
        self.mutate_image_events_in_clip(target, |e| {
            e.fade_out_beats = beats.clamp(0.0, e.event_length_beats.max(0.0));
        });
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_image_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    /// image inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current image
    /// clip に同期する純 marker (= image 編集パス各所から呼ばれる)。 target
    /// が image clip を解決できなければ `None` 化する。
    pub(crate) fn resync_clip_image_event_edit_buffers(&mut self, target: ClipRef) {
        let resolved = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song_doc.song().clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Image(_)));
        self.ui_ephemeral.clip_edit_buffer_target = if resolved { Some(target) } else { None };
    }

    /// `target` が指す clip が `ClipContent::Image` か。 commit / fade /
    /// mute handler の kind dispatch で使う。 範囲外 / 別 variant は false。
    pub fn is_image_clip(&self, target: ClipRef) -> bool {
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        matches!(
            self.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Image(_))
        )
    }

    /// audio clip 判定。 `target` が指す clip が `ClipContent::Audio` か。
    /// MIDI / Vocal / 範囲外は false。 Audio Editor の open 判定で使う。
    pub fn is_audio_clip(&self, target: ClipRef) -> bool {
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        matches!(
            self.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Audio(_))
        )
    }

    /// ピアノロール対象 (= MIDI content) クリップか。歌唱 (VOICEVOX) クリップも
    /// MIDI content なので true (歌詞付き note としてピアノロールに出る)。範囲外 / 非 MIDI は false。
    pub fn is_midi_clip(&self, target: ClipRef) -> bool {
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        matches!(
            self.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Midi(_))
        )
    }

}
