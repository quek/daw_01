//! handler::clip_events — clip 内 audio/image/text event の field 編集 + font picker
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::TimedEvent;

impl AppData {
    /// `target` clip の編集が効く最初の片の `reversed` 値を読む (`handler::clip_window`)。 audio で
    /// ない / 見えている event が無いなら `false`。 メニューの toggle 用。
    pub(crate) fn is_clip_audio_event_reversed(&self, target: ClipKey) -> bool {
        self.audio_edit_targets(target)
            .and_then(|t| self.clip_edit_anchor(t, common::model::ClipContent::audio_events, None))
            .is_some_and(|(e, _)| e.reversed)
    }

    /// `AudioEvent.reversed` を更新 (`docs/plan_audio_clip.md` §3.8)。
    /// audio_editor で event を選択中ならその event、 さもなくば窓に見えている片
    /// (ひと続きは 1 つの take として逆にする、`handler::clip_window`)。
    pub(crate) fn set_clip_audio_event_reversed(&mut self, target: ClipKey, reversed: bool) {
        self.mutate_audio_event_mapping_in_clip(target, |e| e.reversed != reversed, |e| e.reversed = reversed);
    }

    /// `targets` の clip が **全て** muted なら `true` (空なら `false`)。`q` の
    /// toggle 方向決定用 (全 muted → unmute、 1 つでも非 muted → 全 mute)。
    pub fn all_clips_muted(&self, targets: &[ClipKey]) -> bool {
        !targets.is_empty()
            && targets.iter().all(|t| {
                self.cur.song_doc.song()
                    .track_by_id(t.track_id)
                    .and_then(|tr| tr.clip_by_id(t.clip_id))
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
            self.cur.song_doc.song()
                .track_by_id(r.track_id)
                .and_then(|t| t.clip_by_id(r.clip_id))
                .and_then(|c| self.cur.song_doc.song().clip_notes(c).get(local).map(|n| n.muted))
                .unwrap_or(false)
        })
    }

    /// clip-level mute (`Clip.muted`) を設定する。MIDI / audio / video / image /
    /// 字幕 / 歌唱すべての content type 共通の単一 SSoT。`q` ショートカット (`SetClipsMuted`)、
    /// 各 inspector の "Mute" トグル (`DiscreteClipEdit::Muted` / `TextMuted`)、単発の
    /// `SetClipMuted` / `SetClipTextMuted` event がすべてここを経由する。変更があれば
    /// `flush_song_sync` で daw_audio へ LoadSong flush し、再生・書き出しに反映する
    /// (is_dirty もそこで立つ)。
    pub(crate) fn set_clip_muted(&mut self, target: ClipKey, muted: bool) {
        self.edit_song_checked(|song| {
            if let Some(track) = song.track_by_id_mut(target.track_id)
                && let Some(clip) = track.clip_by_id_mut(target.clip_id)
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
                        song.notes_in_clip_mut(r)
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
        target: ClipKey,
        mode: common::model::StretchMode,
    ) {
        self.mutate_audio_event_mapping_in_clip(target, |e| e.stretch_mode != mode, |e| e.stretch_mode = mode);
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
    pub(crate) fn auto_warp_clip(&mut self, target: ClipKey) {
        let Some((content_id, targets)) = self.audio_edit_targets(target) else {
            return;
        };
        // Phase A: 窓の中でひと続きの片ごとに、source range + **take** の配置 beat 長 (immutable borrow)。
        // warp marker の拍は take の座標なので、ひと続きの片には take 全体で 1 組を掛けて片同士で揃える。
        let Some(runs) = self.audio_runs(content_id, &targets) else {
            return;
        };

        // Phase B: onset 検出 → grid snap warp markers (OFF-RT)。
        let mut results: Vec<(Vec<usize>, Vec<common::model::BeatMarker>)> = Vec::new();
        for (run, e) in runs {
            let Some(buf) = self.cur.media.audio_source_cache.get(e.source_id) else {
                continue;
            };
            let (start, end, length_beats) = (e.source_start_frames, e.source_end_frames, e.take_length_beats());
            let source_len = end.saturating_sub(start);
            let mono = buf.downmix_mono(start.min(buf.frames) as usize, end.min(buf.frames) as usize);
            if mono.is_empty() || source_len == 0 || length_beats <= 0.0 {
                continue;
            }
            let onsets = common::onset::detect_onsets(&mono, buf.sample_rate, 0.5);
            let markers = common::audio_render::warp_markers_from_onsets(&onsets, start, source_len, length_beats, 4);
            // anchor 2 件のみ = transient 無し → 空で uniform stretch を維持。
            results.push((run, if markers.len() > 2 { markers } else { Vec::new() }));
        }
        if results.is_empty() {
            return;
        }

        // Phase C: 書き戻し (warp 有効な take は Stretch mode へ) + engine 再 sync。
        let warped = results.iter().filter(|(_, m)| !m.is_empty()).count();
        let changed = self.edit_song(move |song| {
            let Some(common::model::ClipContent::Audio(a)) = song.clip_contents.get_mut(&content_id) else {
                return false;
            };
            for (run, markers) in results {
                common::model::edit_run(&mut a.events, &run, |ev| {
                    if !markers.is_empty() {
                        ev.stretch_mode = common::model::StretchMode::Stretch;
                    }
                    ev.beat_markers = markers;
                });
            }
            true
        });
        if changed == Some(true) {
            self.ui_ephemeral.status_message = format!("Auto-Warp: {warped} event を beat grid に整列");
        }
    }

    /// content `content_id` の audio event `targets` を、窓の中でひと続きの片ごとに (index 列, つないだ event)。
    fn audio_runs(
        &self,
        content_id: common::model::ContentId,
        targets: &[usize],
    ) -> Option<Vec<(Vec<usize>, common::model::AudioEvent)>> {
        let events = self.cur.song_doc.song().clip_contents.get(&content_id)?.audio_events()?;
        Some(
            common::model::piece_runs(events, targets)
                .into_iter()
                .filter_map(|run| common::model::joined_run(events, &run).map(|e| (run, e)))
                .collect(),
        )
    }

    /// B1 (r.md #8): Slice 切替時に GUI decoded buffer から transient を検出して
    /// `AudioEvent.onsets` (= slice trigger 位置、 `source_start_frames` 起点 0
    /// base、 `slice_sample_at` の contract に一致) を埋める。 既に onsets を持つ
    /// event は前回検出 / 将来の user 編集を尊重して skip。 OFF-RT (buffer を 1 回
    /// scan)。 buffer 未 decode の event は skip (= 空 onsets で Raw 等価のまま)。
    pub(crate) fn detect_onsets_for_clip(&mut self, target: ClipKey) {
        let Some((content_id, targets)) = self.audio_edit_targets(target) else {
            return;
        };
        // Phase A: 検出対象 (onsets 空) のひと続きの片と source range を集める (immutable borrow)。
        let Some(runs) = self.audio_runs(content_id, &targets) else {
            return;
        };

        // Phase B: decoded buffer を mono downmix して OFF-RT 検出。
        let mut results: Vec<(Vec<usize>, Vec<u64>)> = Vec::new();
        for (run, e) in runs.into_iter().filter(|(_, e)| e.onsets.is_empty()) {
            let Some(buf) = self.cur.media.audio_source_cache.get(e.source_id) else {
                continue;
            };
            let mono = buf.downmix_mono(
                e.source_start_frames.min(buf.frames) as usize,
                e.source_end_frames.min(buf.frames) as usize,
            );
            if mono.is_empty() {
                continue;
            }
            results.push((run, common::onset::detect_onsets(&mono, buf.sample_rate, 0.5)));
        }
        if results.is_empty() {
            return;
        }

        // Phase C: onsets を書き戻し audio engine へ再 sync (mutable borrow)。
        let _ = self.edit_song(move |song| {
            if let Some(common::model::ClipContent::Audio(a)) = song.clip_contents.get_mut(&content_id) {
                for (run, onsets) in results {
                    common::model::edit_run(&mut a.events, &run, |e| e.onsets = onsets);
                }
            }
        });
    }

    pub(crate) fn set_clip_audio_event_gain_db(&mut self, target: ClipKey, gain_db: f32) {
        let gain_db = gain_db.clamp(-80.0, 24.0);
        self.mutate_audio_events_in_clip(target, |e| e.gain_db = gain_db);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_pan(&mut self, target: ClipKey, pan: f32) {
        let pan = pan.clamp(-1.0, 1.0);
        self.mutate_audio_events_in_clip(target, |e| e.pan = pan);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_pitch_semitones(&mut self, target: ClipKey, semitones: f32) {
        // 範囲の SSoT は `common::model::PITCH_SEMITONES_LIMIT`
        // (inspector の range / 貼り付け sanitize も同じ定数を引く)。
        let semitones =
            common::model::clamp_semitones(semitones, common::model::PITCH_SEMITONES_LIMIT);
        self.mutate_audio_event_mapping_in_clip(
            target,
            |e| e.pitch_semitones != semitones,
            |e| e.pitch_semitones = semitones,
        );
        self.resync_clip_audio_event_edit_buffers(target);
    }

    /// r.md #40: スペクトル包絡 (フォルマント) の移調量。 `0` の意味は
    /// stretch mode で変わる (`common::model::AudioEvent::formant_semitones` の表)。
    pub(crate) fn set_clip_audio_event_formant_semitones(
        &mut self,
        target: ClipKey,
        semitones: f32,
    ) {
        let semitones =
            common::model::clamp_semitones(semitones, common::model::FORMANT_SEMITONES_LIMIT);
        self.mutate_audio_events_in_clip(target, |e| e.formant_semitones = semitones);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    /// audio inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current audio
    /// clip に同期する純 marker (= 多数の audio 編集パス / song 差し替えから
    /// 呼ばれる)。 target が audio clip を解決できなければ `None` 化する。
    pub(crate) fn resync_clip_audio_event_edit_buffers(&mut self, target: ClipKey) {
        let resolved = self
            .cur.song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .and_then(|c| self.cur.song_doc.song().clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Audio(_)));
        self.cur.peph.clip_edit_buffer_target = if resolved { Some(target) } else { None };
    }


    /// r.md #38: clip 内の **1 event (を含む窓の中のひと続き)** の fade を content 種別に依らず書き換える。
    ///
    /// アレンジ画面の fade 角 drag はこれを使う。 audio / video / image / text の
    /// 4 種は同じ fade フィールドを持ち、 適用側も同じ curve 式を通るので、
    /// 種別ごとの setter を 4 本用意する必要はない
    /// (`ClipContent::set_window_fade` が唯一の書き込み口。 掴み所はひと続きの外側の端にだけ出る)。
    ///
    /// clamp は caller (`f`) の責務。 `EventFade::len_beats` が上限。
    pub(crate) fn set_clip_event_fade(
        &mut self,
        target: crate::app_types::ClipEventRef,
        f: impl FnOnce(common::model::EventFade) -> common::model::EventFade,
    ) {
        let Some((content_id, window)) =
            self.cur.song_doc.song().clip_by_key(target.clip).map(|c| (c.content_id, c.content_window()))
        else {
            return;
        };
        let index = target.event as usize;
        self.edit_song_checked(|song| {
            song.clip_contents
                .get_mut(&content_id)
                .is_some_and(|c| c.set_window_fade(window, index, f))
        });
        // audio の inspector edit buffer はこの clip の値を映すので resync する
        // (他 content 種別の setter は自前の resync を持つが、 fade は arrangement 側
        // からしか来ないので audio 側だけで十分)。
        self.resync_clip_audio_event_edit_buffers(target.clip);
    }

    pub(crate) fn set_clip_audio_event_fade_in_beats(&mut self, target: ClipKey, beats: f64) {
        // r.md #38: 上限は **event 長**。 音 (`audio_clip_renderer`) は event 長基準で
        // fade を掛けるので、 clip 長で clamp すると clip より短い event
        // (trim / split 後) で fade がフルゲインに到達せず絵と音がずれる。
        // 掛け直した fade は左端から始まる (分割の片が持っていたランプの続きは捨てる)。
        self.mutate_audio_events_in_clip(target, |e| {
            e.set_edge_fade_in(beats.clamp(0.0, e.event_length_beats.max(0.0)));
        });
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_fade_out_beats(&mut self, target: ClipKey, beats: f64) {
        self.mutate_audio_events_in_clip(target, |e| {
            e.set_edge_fade_out(beats.clamp(0.0, e.event_length_beats.max(0.0)));
        });
        self.resync_clip_audio_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_audio_event_fade_in_curve(
        &mut self,
        target: ClipKey,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_audio_event_fade_out_curve(
        &mut self,
        target: ClipKey,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    // -------- Image event editors (`docs/plan_image_overlay.md` §4 P4) ----

    pub(crate) fn set_clip_image_event_x(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.x = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_y(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.y = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_w(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.w = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_h(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.h = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_opacity(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.opacity = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_rotation_radians(&mut self, target: ClipKey, value: f32) {
        // -π..=π で wrap して保存。 lane override 経由でも同じ wrap が
        // composite で適用される (= preview 表示は modulo 2π)。
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_image_events_in_clip(target, |e| e.rotation_radians = wrapped);
        self.resync_clip_image_event_edit_buffers(target);
    }

    /// r.md #98: 左右反転。 discrete toggle なので edit buffer の resync は不要
    /// (数値 field ではない)。 image clip 以外は `mutate_image_events_in_clip` が no-op。
    pub(crate) fn set_clip_image_event_flip_h(&mut self, target: ClipKey, value: bool) {
        self.mutate_image_events_in_clip(target, |e| e.flip_h = value);
    }

    /// r.md #98: 上下反転。 `set_clip_image_event_flip_h` と同じ。
    pub(crate) fn set_clip_image_event_flip_v(&mut self, target: ClipKey, value: bool) {
        self.mutate_image_events_in_clip(target, |e| e.flip_v = value);
    }

    /// docs/plan_text_overlay.md §4 P6: image と同 idiom の text event
    /// setter 群。 drag / inspector commit / lane override 経由のいずれも
    /// このパスで TextEvent.field を直接書く。 書くのは窓に見えている片 (ひと続きは 1 つとして、
    /// `handler::clip_window`) で、値が変わらなければ履歴にも dirty にも残さない。
    pub(crate) fn mutate_text_events_in_clip<F>(&mut self, target: ClipKey, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::TextEvent),
    {
        let Some(targets) = self.clip_shown_targets(target, common::model::ClipContent::text_events) else {
            return false;
        };
        self.edit_event_runs(targets, common::model::ClipContent::text_events_mut, &mut f)
    }

    pub(crate) fn set_clip_text_event_x(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.x = value);
    }

    pub(crate) fn set_clip_text_event_y(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.y = value);
    }

    pub(crate) fn set_clip_text_event_w(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.w = value);
    }

    pub(crate) fn set_clip_text_event_h(&mut self, target: ClipKey, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.h = value);
    }

    pub(crate) fn set_clip_text_event_rotation_radians(&mut self, target: ClipKey, value: f32) {
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_text_events_in_clip(target, |e| e.rotation_radians = wrapped);
    }

    pub(crate) fn set_clip_text_event_content(&mut self, target: ClipKey, value: String) {
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

    pub(crate) fn set_clip_text_event_font_family(&mut self, target: ClipKey, value: String) {
        self.mutate_text_events_in_clip(target, |e| e.font_family = value.clone());
        self.resync_clip_text_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_text_event_align(&mut self, target: ClipKey, value: common::model::TextAlign) {
        self.mutate_text_events_in_clip(target, |e| e.align = value);
    }

    pub(crate) fn set_clip_text_event_fade_in_curve(
        &mut self,
        target: ClipKey,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_text_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_text_event_fade_out_curve(
        &mut self,
        target: ClipKey,
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
        target: ClipKey,
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
                    e.set_edge_fade_in(v.clamp(0.0, e.event_length_beats.max(0.0)));
                });
            }
            F::FadeOutBeats => {
                let v = f64::from(value);
                self.mutate_text_events_in_clip(target, |e| {
                    e.set_edge_fade_out(v.clamp(0.0, e.event_length_beats.max(0.0)));
                });
            }
        }
        self.resync_clip_text_event_edit_buffers(target);
    }

    pub(crate) fn commit_clip_text_content_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.cur.peph.clip_text_content_edit_text.clone();
        self.set_clip_text_event_content(target, value);
    }

    pub(crate) fn commit_clip_text_font_family_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.cur.peph.clip_text_font_family_edit_text.clone();
        self.set_clip_text_event_font_family(target, value);
    }

    // -------- Font picker -------------------------------------

    /// 編集対象 text クリップの現在のフォント名 (窓に見えている最初の片)。text クリップで
    /// なければ `None`。
    pub(crate) fn clip_text_font_family(&self, target: ClipKey) -> Option<String> {
        self.text_first_event(target, |e| e.font_family.clone())
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
        self.cur.song_doc.begin_gesture();
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
        self.cur.song_doc.end_gesture();
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
        self.cur.song_doc.end_gesture();
    }

    /// docs/plan_text_overlay.md §4 P5: clip 切替 / Undo / Redo / lane
    /// override 変化等で文字列 edit buffer (content / font_family) を current
    /// TextEvent の値で再構築。 25 numeric field は scrubable_number
    /// 化され現値を summary から直接読むため、 数値 buffer の再生成は不要に
    /// なった。 target が Text variant でないなら文字列 buffer を空にして
    /// `clip_edit_buffer_target` を `None`。
    pub(crate) fn resync_clip_text_event_edit_buffers(&mut self, target: ClipKey) {
        let event_snapshot = self.text_first_event(target, |e| (e.text.clone(), e.font_family.clone()));
        let Some((text, font_family)) = event_snapshot else {
            self.cur.peph.clip_text_content_edit_text.clear();
            self.cur.peph.clip_text_font_family_edit_text.clear();
            self.cur.peph.clip_edit_buffer_target = None;
            return;
        };
        self.cur.peph.clip_text_content_edit_text = text;
        self.cur.peph.clip_text_font_family_edit_text = font_family;
        self.cur.peph.clip_edit_buffer_target = Some(target);
    }

    /// docs/plan_text_overlay.md §4 P5: text inspector が表示する
    /// snapshot (= image idiom)。 selected_clip が Text variant の clip
    /// を指していて、 first event があれば `Some` を返す。 各 numeric
    /// field の `*_automated` は対応する TextBuiltin lane が track に
    /// 存在するか。
    pub fn inspector_text_event_summary(&self) -> Option<InspectorTextEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.cur.song_doc.song().track_by_id(cref.track_id)?;
        let clip = track.clip_by_id(cref.clip_id)?;
        // 値は窓に見えている最初のひと続きから読む (`handler::clip_window`)。
        let events_of = common::model::ClipContent::text_events;
        let (event, fade) = self.clip_edit_anchor(self.clip_shown_targets(cref, events_of)?, events_of, None)?;
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
            fade_in_curve: fade.fade_in_curve,
            fade_out_curve: fade.fade_out_curve,
            automated,
            fade_max_beats: fade.len_beats,
            event: common::model::TextEvent {
                fade_in_beats: fade.visible_fade_in_beats(),
                fade_out_beats: fade.visible_fade_out_beats(),
                ..event.clone()
            },
        })
    }

    pub(crate) fn set_clip_image_event_fade_in_beats(&mut self, target: ClipKey, beats: f64) {
        // r.md #38: image_compose も event 長基準で fade を適用するので上限は event 長。
        self.mutate_image_events_in_clip(target, |e| {
            e.set_edge_fade_in(beats.clamp(0.0, e.event_length_beats.max(0.0)));
        });
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_fade_out_beats(&mut self, target: ClipKey, beats: f64) {
        self.mutate_image_events_in_clip(target, |e| {
            e.set_edge_fade_out(beats.clamp(0.0, e.event_length_beats.max(0.0)));
        });
        self.resync_clip_image_event_edit_buffers(target);
    }

    pub(crate) fn set_clip_image_event_fade_in_curve(
        &mut self,
        target: ClipKey,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    pub(crate) fn set_clip_image_event_fade_out_curve(
        &mut self,
        target: ClipKey,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    /// image inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current image
    /// clip に同期する純 marker (= image 編集パス各所から呼ばれる)。 target
    /// が image clip を解決できなければ `None` 化する。
    pub(crate) fn resync_clip_image_event_edit_buffers(&mut self, target: ClipKey) {
        let resolved = self
            .cur.song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .and_then(|c| self.cur.song_doc.song().clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Image(_)));
        self.cur.peph.clip_edit_buffer_target = if resolved { Some(target) } else { None };
    }

    /// `target` が指す clip が `ClipContent::Image` か。 commit / fade /
    /// mute handler の kind dispatch で使う。 範囲外 / 別 variant は false。
    pub fn is_image_clip(&self, target: ClipKey) -> bool {
        let Some(track) = self.cur.song_doc.song().track_by_id(target.track_id) else {
            return false;
        };
        let Some(clip) = track.clip_by_id(target.clip_id) else {
            return false;
        };
        matches!(
            self.cur.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Image(_))
        )
    }

    /// audio clip 判定。 `target` が指す clip が `ClipContent::Audio` か。
    /// MIDI / Vocal / 範囲外は false。 Audio Editor の open 判定で使う。
    pub fn is_audio_clip(&self, target: ClipKey) -> bool {
        let Some(track) = self.cur.song_doc.song().track_by_id(target.track_id) else {
            return false;
        };
        let Some(clip) = track.clip_by_id(target.clip_id) else {
            return false;
        };
        matches!(
            self.cur.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Audio(_))
        )
    }

    /// ピアノロール対象 (= MIDI content) クリップか。歌唱 (VOICEVOX) クリップも
    /// MIDI content なので true (歌詞付き note としてピアノロールに出る)。範囲外 / 非 MIDI は false。
    pub fn is_midi_clip(&self, target: ClipKey) -> bool {
        let Some(track) = self.cur.song_doc.song().track_by_id(target.track_id) else {
            return false;
        };
        let Some(clip) = track.clip_by_id(target.clip_id) else {
            return false;
        };
        matches!(
            self.cur.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Midi(_))
        )
    }

}
