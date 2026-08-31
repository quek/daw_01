//! handler::audio_editor — Audio Editor の view + event 編集 + auto fade/crossfade
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use std::path::{Path, PathBuf};
use common::model::{AudioEvent};
use crate::import_audio;

impl AppData {
    /// audio clip ダブルクリックで Audio Editor を開く。 `target` が
    /// 非 audio (MIDI / Vocal / 範囲外) なら silent no-op。 bottom_panel
    /// を tab 1 (= 通常 Piano Roll、 audio_editor_clip is Some なら
    /// audio_editor view に切り替わる) に揃える。
    pub(crate) fn open_audio_editor(&mut self, target: ClipKey) {
        if !self.is_audio_clip(target) {
            return;
        }
        // 別 clip を開くときは前 clip の event 選択 (クリップ内 index) が stale に
        // なるので clear する (同 clip の再 open は選択を保持)。 = close / undo と同方針。
        if self.ui_ephemeral.audio_editor_clip != Some(target) {
            self.set_audio_event_selection(&[]);
        }
        self.ui_ephemeral.audio_editor_clip = Some(target);
        self.ui_prefs.bottom_panel = 1;
        // per-clip 記憶。 初回 (entry 無し) のクリップだけ「全体表示」の初期 view を
        // 入れる。 既に記憶があればその view を復元 (= map をそのまま読む)。
        let Some(key) = self.live_clip_key(target) else { return };
        if !self.ui_prefs.audio_editor_views.contains_key(&key) {
            // r.md #44: view は content-local 軸なので、初期 view は clip の窓
            // (`[content_offset_beats, +length_beats)`) をそのまま全体表示する。
            let (start_beat, len_beats) = self
                .song_doc.song()
                .track_by_id(target.track_id)
                .and_then(|t| t.clip_by_id(target.clip_id))
                .map_or((0.0, 0.0), |c| (c.content_offset_beats, c.length_beats));
            self.ui_prefs.audio_editor_views.insert(
                key,
                common::model::AudioEditorViewState {
                    start_beat,
                    len_beats: len_beats.max(0.0),
                },
            );
        }
    }

    pub(crate) fn close_audio_editor(&mut self) {
        // view 状態は `audio_editor_views` に残す (= 次回 open で復元)。
        self.ui_ephemeral.audio_editor_clip = None;
        self.set_audio_event_selection(&[]);
        self.ui_ephemeral.audio_editor_hover_beat_in_clip = None;
        // 面そのものが消えたので last-wins タグも降ろす。 残すと
        // 「閉じた audio editor の面」 を指したまま `edit_surface` が空判定で
        // 落ちるだけの死んだタグになり、 Delete が None に倒れて効かなくなる。
        if self.selection.last_edit_select == Some(EditSurface::AudioEvents) {
            self.selection.last_edit_select = None;
        }
    }

    /// Audio Editor の編集対象を指す **安定 `ClipKey`**。 track / clip の Vec が
    /// 詰まる編集の **直前** に退避しておき、 編集後に
    /// [`Self::reanchor_audio_editor`] へ渡す。
    pub(crate) fn audio_editor_target_key(&self) -> Option<common::model::ClipKey> {
        self.ui_ephemeral
            .audio_editor_clip
            .and_then(|r| self.live_clip_key(r))
    }

    /// `ui_ephemeral.audio_editor_clip` は安定 id (`ClipKey`) なので、トラック削除 /
    /// undo / redo / load で Vec が詰まっても**別のクリップを指すことはない**
    /// (index 時代はここが黙ってずれ、エディタ上の Delete が無関係なクリップの
    /// イベントを消していた)。残る失敗は「対象そのものが消える / 種別が変わる」で、
    /// それをここで畳む。
    ///
    /// 編集前に退避した key (`audio_editor_target_key`) で引き直し、同じクリップが
    /// 生きていて **まだ audio なら開いたまま**、消えた / audio でなくなった /
    /// そもそも key が取れなかったなら閉じる。
    pub(crate) fn reanchor_audio_editor(&mut self, key: Option<common::model::ClipKey>) {
        if self.ui_ephemeral.audio_editor_clip.is_none() {
            return;
        }
        let Some(key) = key else {
            self.close_audio_editor();
            return;
        };
        let still_audio = self
            .clip_at(key)
            .map(|c| c.content_id)
            .and_then(|cid| self.song_doc.song().clip_contents.get(&cid))
            .is_some_and(|c| matches!(c, common::model::ClipContent::Audio(_)));
        match self.live_clip_key(key) {
            Some(r) if still_audio => self.ui_ephemeral.audio_editor_clip = Some(r),
            _ => self.close_audio_editor(),
        }
    }

    /// Audio Editor 水平 scroll: `view_start_beat` を `[0, total - view_len]`
    /// で clamp。 `audio_editor_clip` が None / clip が解決できない場合は no-op。
    pub(crate) fn set_audio_editor_scroll(&mut self, new_start: f64) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else { return };
        let Some(key) = self.live_clip_key(target) else { return };
        // r.md #44: view は content-local 軸で、clip が見せる窓
        // `[content_offset_beats, +length_beats)` に clamp する。
        let Some((min_start, total)) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| (c.content_offset_beats, c.length_beats.max(0.0)))
        else {
            return;
        };
        // entry 無し = まだ全体表示 → view_len は total 扱い。
        let view_len = self
            .ui_prefs.audio_editor_views
            .get(&key)
            .map_or(total, |v| v.len_beats)
            .max(0.0)
            .min(total);
        let max_start = min_start + (total - view_len).max(0.0);
        self.ui_prefs.audio_editor_views.entry(key).or_default().start_beat =
            new_start.clamp(min_start, max_start);
    }

    /// Audio Editor zoom: `view_start_beat` + `view_len_beats` を一括設定。
    /// `view_len` は `[MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, clip.length]`、
    /// `view_start` は `[0, clip.length - view_len]` で clamp。
    pub(crate) fn set_audio_editor_zoom(&mut self, new_start: f64, new_len: f64) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else { return };
        let Some(key) = self.live_clip_key(target) else { return };
        let Some((min_start, total)) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| (c.content_offset_beats, c.length_beats.max(0.0)))
        else {
            return;
        };
        let len = new_len.clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total.max(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS));
        let max_start = min_start + (total - len).max(0.0);
        let entry = self.ui_prefs.audio_editor_views.entry(key).or_default();
        entry.start_beat = new_start.clamp(min_start, max_start);
        entry.len_beats = len;
    }

    /// PR-D 段階 1: Audio Editor で開いている clip + 選択中 event を
    /// Duplicate (= 同 source の event を直後に複製、 spec §3.10.2 の
    /// `Ctrl+D`)。 audio_editor_clip と audio_editor_selected_event の
    /// どちらかが None なら no-op。 新 event は src.event_start +
    /// src.event_length_beats の位置に配置、 同 source / 同パラメータ。
    /// clip.length_beats は新 event の終端を超えないように自動拡張。
    /// selection は新 event index に進む。
    pub(crate) fn duplicate_audio_editor_event(&mut self) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return;
        };
        let Some(idx) = self.audio_editor_anchor_event() else {
            return;
        };
        let Some(Some(insert_at)) = self.edit_song(|song| {
            let content_id = song.clip_by_key(target)?.content_id;
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return None;
            };
            let src = audio.events.get(idx).cloned()?;
            let new_start = src.event_start_in_clip_beats + src.event_length_beats;
            let mut new_event = src.clone();
            new_event.event_start_in_clip_beats = new_start;
            // clone は src の非 0 `id` も複製する。 per-content 一意 id 不変条件
            // (invariant #1) を守るため新規採番する — さもないと同 content に同 id の
            // event が 2 つでき (`ensure_element_ids` も既存非 0 id は再採番しないので
            // save/reload で残存)、 id addressing が壊れる。
            new_event.id = audio.alloc_event_id();
            let insert_at = idx + 1;
            if insert_at >= audio.events.len() {
                audio.events.push(new_event);
            } else {
                audio.events.insert(insert_at, new_event);
            }
            // clip.length_beats を必要に応じて拡張 (= 新 event の右端を含むよう
            // に)。 元 length より長くなる場合のみ更新。
            let needed = new_start + src.event_length_beats;
            // r.md #44: `needed` は content-local。 clip の窓の末尾
            // (`content_offset_beats + length_beats`) を基準に伸ばす。
            if let Some(clip) = song.clip_by_key_mut(target)
                && needed > clip.content_offset_beats + clip.length_beats
            {
                clip.length_beats = needed - clip.content_offset_beats;
            }
            Some(insert_at)
        }) else {
            return;
        };
        self.set_audio_event_selection(&[insert_at]);
        if self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor で event の clip 内位置を変更 (= 中央
    /// drag 移動)。 `event_start_in_clip_beats` を `new_start_beats`
    /// (clamp 0..) に設定。 範囲外 / 非 audio clip / event_idx 範囲外
    /// なら no-op。 clip.length_beats は新 event 終端を含むよう自動拡張。
    pub(crate) fn set_audio_event_start(
        &mut self,
        target: ClipKey,
        event_idx: usize,
        new_start_beats: f64,
    ) {
        let changed = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(target.track_id) else {
                return false;
            };
            let Some(clip) = track.clip_by_id_mut(target.clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            let Some(event) = audio.events.get_mut(event_idx) else {
                return false;
            };
            let new_start = new_start_beats.max(0.0);
            event.event_start_in_clip_beats = new_start;
            let needed = new_start + event.event_length_beats;
            // r.md #44: `needed` は content-local。 clip の窓の末尾
            // (`content_offset_beats + length_beats`) を基準に伸ばす。
            if let Some(clip) = song.clip_by_key_mut(target)
                && needed > clip.content_offset_beats + clip.length_beats
            {
                clip.length_beats = needed - clip.content_offset_beats;
            }
            true
        });
        if changed && self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor で event 端 trim (= 左右端 drag)。
    /// `side == Left` で左端 trim (= event_start_in_clip_beats +
    /// event_length_beats + source_start_frames を delta で連動)、
    /// `side == Right` で右端 trim (= event_length_beats +
    /// source_end_frames を連動)。 source の sample_rate で
    /// delta_beats → frames 変換 (bpm = self.song_doc.song().bpm)。 source 境界
    /// (0..total_frames) と event_length_beats > 0 を保つ clamp 込み。
    pub(crate) fn set_audio_event_trim(
        &mut self,
        target: ClipKey,
        event_idx: usize,
        side: AudioEventTrimSide,
        delta_beats: f64,
    ) {
        let bpm = self.song_doc.song().bpm.max(1.0) as f64;
        // source 情報を先に snapshot (= 後の mut borrow と分離)。
        let (sr_hz, total_frames) = {
            let Some(track) = self.song_doc.song().track_by_id(target.track_id) else {
                return;
            };
            let Some(clip) = track.clip_by_id(target.clip_id) else {
                return;
            };
            let Some(common::model::ClipContent::Audio(audio)) =
                self.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                return;
            };
            let Some(event) = audio.events.get(event_idx) else {
                return;
            };
            let Some(audio_source) = self.song_doc.song().media.audio_sources.get(&event.source_id) else {
                return;
            };
            (audio_source.sample_rate as f64, audio_source.frames)
        };
        let delta_frames = (delta_beats * 60.0 / bpm * sr_hz).round() as i64;

        let changed = self.edit_song_checked(|song| {
            let Some(track) = song.track_by_id_mut(target.track_id) else {
                return false;
            };
            let Some(clip) = track.clip_by_id_mut(target.clip_id) else {
                return false;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            let Some(event) = audio.events.get_mut(event_idx) else {
                return false;
            };

            const MIN_LEN_BEATS: f64 = 1e-4;
            match side {
                AudioEventTrimSide::Left => {
                    // delta_beats > 0 で右に縮める (= start を遅らせる)、
                    // < 0 で左に伸ばす。 ただし event_length が MIN_LEN を
                    // 切らないよう先に clamp。
                    let max_inset = (event.event_length_beats - MIN_LEN_BEATS).max(0.0);
                    let dbeats = delta_beats.clamp(
                        -event.event_start_in_clip_beats,
                        max_inset,
                    );
                    let dframes = (dbeats * 60.0 / bpm * sr_hz).round() as i64;
                    let new_start_in_clip = event.event_start_in_clip_beats + dbeats;
                    let new_length = event.event_length_beats - dbeats;
                    let new_source_start = (event.source_start_frames as i64 + dframes)
                        .max(0)
                        .min(event.source_end_frames as i64) as u64;
                    event.event_start_in_clip_beats = new_start_in_clip;
                    event.event_length_beats = new_length.max(MIN_LEN_BEATS);
                    event.source_start_frames = new_source_start;
                    let _ = delta_frames;
                }
                AudioEventTrimSide::Right => {
                    // delta_beats > 0 で右に伸ばす、 < 0 で縮める。 縮める
                    // 側は event_length が MIN_LEN を切らないよう clamp、
                    // 伸ばす側は source_end_frames が total_frames を超え
                    // ないよう clamp。
                    let max_grow_frames = total_frames as i64 - event.source_end_frames as i64;
                    let max_grow_beats =
                        (max_grow_frames as f64) / sr_hz * bpm / 60.0;
                    let min_shrink_beats = -(event.event_length_beats - MIN_LEN_BEATS).max(0.0);
                    let dbeats = delta_beats.clamp(min_shrink_beats, max_grow_beats);
                    let dframes = (dbeats * 60.0 / bpm * sr_hz).round() as i64;
                    let new_length = event.event_length_beats + dbeats;
                    let new_source_end = ((event.source_end_frames as i64 + dframes)
                        .max(event.source_start_frames as i64)
                        .min(total_frames as i64)) as u64;
                    event.event_length_beats = new_length.max(MIN_LEN_BEATS);
                    event.source_end_frames = new_source_end;
                }
            }

            let needed = event.event_start_in_clip_beats + event.event_length_beats;
            // r.md #44: `needed` は content-local。 clip の窓の末尾
            // (`content_offset_beats + length_beats`) を基準に伸ばす。
            if let Some(clip) = song.clip_by_key_mut(target)
                && needed > clip.content_offset_beats + clip.length_beats
            {
                clip.length_beats = needed - clip.content_offset_beats;
            }
            true
        });
        if changed && self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor の空白領域 file drop で新 event 追加。
    /// `import_audio::import_one` で decode + audio source 登録、 既存
    /// audio clip に新 event を `position_in_clip_beats` (clamp 0..) に
    /// 配置。 失敗時は status_message にエラー、 selection は新 event に
    /// 移す。 clip.length_beats は新 event 終端を含むよう自動拡張。
    pub(crate) fn add_audio_event_from_file(
        &mut self,
        target: ClipKey,
        path: PathBuf,
        position_in_clip_beats: f64,
    ) {
        if !self.is_audio_clip(target) {
            self.ui_ephemeral.status_message = "Audio Editor: 対象 clip が audio ではないため event 追加できません".into();
            return;
        }
        let project_dir: Option<PathBuf> = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let imported = match import_audio::import_one(&path, project_dir.as_deref()) {
            Ok(i) => i,
            Err(e) => {
                self.ui_ephemeral.status_message = format!("Audio event 追加 失敗: {}: {e}", path.display());
                return;
            }
        };
        let bpm = self.song_doc.song().bpm;
        let length_beats =
            frames_to_beats(imported.buffer.frames, imported.buffer.sample_rate, bpm);
        let display_name = imported.display_name.clone();

        let Some(source_id) = self.edit_song(|song| {
            let source_id = song.alloc_audio_source_id();
            song.media.audio_sources.insert(source_id, imported.source);
            source_id
        }) else {
            return;
        };
        self.media.audio_source_cache
            .insert(source_id, imported.buffer.clone());

        let position = position_in_clip_beats.max(0.0);
        let source_end_frames = imported.buffer.frames;
        let Some(Some(new_idx)) = self.edit_song(|song| {
            let content_id = song.clip_by_key(target)?.content_id;
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return None;
            };
            let mut new_event = AudioEvent {
                source_id,
                event_start_in_clip_beats: position,
                event_length_beats: length_beats,
                source_start_frames: 0,
                source_end_frames,
                ..AudioEvent::default()
            };
            // 安定 id を採番する (`AudioEvent::default()` は 0 sentinel)。 波形の LOD
            // キャッシュ等が id でアドレスするので、 同一 content 内で衝突させない。
            new_event.id = audio.alloc_event_id();
            audio.events.push(new_event);
            let new_idx = audio.events.len() - 1;
            let needed = position + length_beats;
            // r.md #44: `needed` は content-local。 clip の窓の末尾
            // (`content_offset_beats + length_beats`) を基準に伸ばす。
            if let Some(clip) = song.clip_by_key_mut(target)
                && needed > clip.content_offset_beats + clip.length_beats
            {
                clip.length_beats = needed - clip.content_offset_beats;
            }
            Some(new_idx)
        }) else {
            return;
        };
        self.set_audio_event_selection(&[new_idx]);
        if self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        self.ui_ephemeral.status_message = format!("Audio event 追加: {display_name}");
    }

    /// Audio Editor の event 選択集合を `indices` で置き換える。 重複を
    /// 除いて格納 (anchor = last なので最後に追加された index が代表)。
    /// 範囲外 index は use 時に `.get` で無視されるのでここでは除外しない
    /// (= n_events を知るための再 resolve を避ける)。 view state、 非 undoable。
    pub(crate) fn set_audio_editor_event_selection(&mut self, indices: Vec<usize>) {
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<usize> = indices.into_iter().filter(|i| seen.insert(*i)).collect();
        self.set_audio_event_selection(&(deduped));
        if !self.selected_audio_event_indices().is_empty() {
            self.selection.last_edit_select = Some(EditSurface::AudioEvents);
        }
    }

    /// Audio Editor で選択中の全 event を削除 (= Delete key、 複数選択
    /// 対応)。 高い index から `remove` して shift を回避。 削除後は
    /// selection を clear。 events が空になっても content は保持。
    pub(crate) fn delete_audio_editor_selection(&mut self) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return;
        };
        let Some(content_id) = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map(|c| c.content_id)
        else {
            return;
        };
        let mut indices: Vec<usize> = self.selected_audio_event_indices();
        indices.sort_unstable();
        indices.dedup();
        if indices.is_empty() {
            return;
        }
        let removed = self.edit_song_checked(move |song| {
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            for &i in indices.iter().rev() {
                if i < audio.events.len() {
                    audio.events.remove(i);
                }
            }
            true
        });
        if !removed {
            return;
        }
        self.set_audio_event_selection(&[]);
        if self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// 全選択 audio clip に短 fade を一括適用 (`docs/plan_audio_clip
    /// .md` §3.5 Auto-Fade)。 fade 長は 4 ms 相当 (= `0.004 * bpm / 60`
    /// beats)、 既存値は上書き。 audio 以外の clip (MIDI / Vocal) と
    /// `selected_clip` がない場合は no-op。
    pub(crate) fn auto_fade_selected_clips(&mut self) {
        let bpm = self.song_doc.song().bpm.max(1.0) as f64;
        let auto_fade_beats = 0.004 * bpm / 60.0; // 4 ms 相当
        let mut applied = 0usize;
        // borrow checker: target list を先に固める。
        let targets: Vec<ClipKey> = self.selected_clip_refs();
        for target in targets {
            let Some(content_id) = self
                .song_doc.song()
                .track_by_id(target.track_id)
                .and_then(|t| t.clip_by_id(target.clip_id))
                .map(|c| c.content_id)
            else {
                continue;
            };
            let did = self.edit_song_checked(move |song| {
                if let Some(common::model::ClipContent::Audio(audio)) =
                    song.clip_contents.get_mut(&content_id)
                {
                    for event in &mut audio.events {
                        // r.md #38: fade の上限は clip 長ではなく **event 長**
                        // (音は event 長基準で fade を掛ける)。 Auto-Crossfade
                        // (`auto_crossfade_selected_clips`) と同じ基準。
                        let fade_beats =
                            auto_fade_beats.min(event.event_length_beats.max(0.0));
                        event.fade_in_beats = fade_beats;
                        event.fade_out_beats = fade_beats;
                    }
                    true
                } else {
                    false
                }
            });
            if did {
                applied += 1;
            }
        }
        if applied > 0 {
            // edit buffer (Inspector) も追従させる。
            if let Some(target) = self.ui_ephemeral.clip_edit_buffer_target {
                self.resync_clip_audio_event_edit_buffers(target);
            }
            self.ui_ephemeral.status_message = format!("Auto-Fade: {applied} 個のクリップに 4 ms fade を適用");
        } else {
            self.ui_ephemeral.status_message = "Auto-Fade: 選択中の audio clip がありません".into();
        }
    }

    /// Auto-Crossfade — **隣接する audio クリップの境界**にクロスフェードを掛ける。
    ///
    /// クリップ同士は重ならない (`Track::clips` の不変条件) ので、境界で音を途切れ
    /// させないには **鳴らす範囲だけ**を境界の向こうへ伸ばす。 1 ペアにつき:
    ///
    /// - 前のクリップ: `xfade_tail_beats = N/2` (境界の先まで鳴らす) + 末尾 event の
    ///   `fade_out_beats = N`
    /// - 次のクリップ: `xfade_lead_beats = N/2` (境界の手前から鳴らす) + 先頭 event の
    ///   `fade_in_beats = N`
    ///
    /// これで境界を中心に左が下がりながら右が上がる = 真のクロスフェードになる
    /// (`docs/plan_range_selection.md` §6.5)。 張り出しは再生側が**隣が実在するときだけ**
    /// 使うので、後でクリップを動かしても音が漏れない。
    ///
    /// Live の「隣接クリップに自動で 4ms が付く」 (§6.8) は入れていない — フェードは
    /// ユーザーが明示的に掛けたときだけ付く。
    pub(crate) fn auto_crossfade_selected_clips(&mut self) {
        // クロスフェード長 (拍)。 4 ms 相当を拍へ換算 (Auto-Fade と同じ尺度)。
        let bpm = f64::from(self.song_doc.song().bpm.max(1.0));
        let xfade_beats = (0.004 * bpm / 60.0).max(1e-4);
        // (track_id, clip_id, start, end, content_id) を集める。
        let mut entries: Vec<(u32, u32, f64, f64, common::model::ContentId)> = Vec::new();
        for target in self.selected_clip_refs() {
            let Some(clip) = self.song_doc.song().clip_by_key(target) else {
                continue;
            };
            let Some(common::model::ClipContent::Audio(_)) =
                self.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            entries.push((
                target.track_id,
                target.clip_id,
                clip.start_beat,
                clip.start_beat + clip.length_beats,
                clip.content_id,
            ));
        }
        if entries.len() < 2 {
            self.ui_ephemeral.status_message =
                "Auto-Crossfade: 隣接判定には audio clip が 2 つ以上必要です".into();
            return;
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.total_cmp(&b.2)));
        let mut pairs: Vec<(u32, u32, u32, common::model::ContentId, common::model::ContentId)> =
            Vec::new();
        for w in entries.windows(2) {
            let (prev_track, prev_id, _, prev_end, prev_content) = w[0];
            let (next_track, next_id, next_start, _, next_content) = w[1];
            // **隣接** = 端が触れている (重なりは不変条件で存在しない)。
            if prev_track != next_track || (next_start - prev_end).abs() > 1e-6 {
                continue;
            }
            pairs.push((prev_track, prev_id, next_id, prev_content, next_content));
        }
        if pairs.is_empty() {
            self.ui_ephemeral.status_message =
                "Auto-Crossfade: 隣接しているペアがありません".into();
            return;
        }
        let applied = pairs.len();
        let half = xfade_beats * 0.5;
        self.edit_song(move |song| {
            for (track_id, prev_id, next_id, prev_content, next_content) in pairs {
                if let Some(clip) = song
                    .track_by_id_mut(track_id)
                    .and_then(|t| t.clip_by_id_mut(prev_id))
                {
                    clip.xfade_tail_beats = half;
                }
                if let Some(clip) = song
                    .track_by_id_mut(track_id)
                    .and_then(|t| t.clip_by_id_mut(next_id))
                {
                    clip.xfade_lead_beats = half;
                }
                // ランプは event 側に持たせる (fade の SSoT は event)。 境界を挟んで
                // 左が下がり右が上がるよう、両側とも長さ N を掛ける。
                if let Some(common::model::ClipContent::Audio(audio)) =
                    song.clip_contents.get_mut(&prev_content)
                    && let Some(last) = audio.events.iter_mut().max_by(|a, b| {
                        (a.event_start_in_clip_beats + a.event_length_beats)
                            .total_cmp(&(b.event_start_in_clip_beats + b.event_length_beats))
                    })
                {
                    last.fade_out_beats = xfade_beats;
                }
                if let Some(common::model::ClipContent::Audio(audio)) =
                    song.clip_contents.get_mut(&next_content)
                    && let Some(first) = audio.events.iter_mut().min_by(|a, b| {
                        a.event_start_in_clip_beats.total_cmp(&b.event_start_in_clip_beats)
                    })
                {
                    first.fade_in_beats = xfade_beats;
                }
            }
        });
        if let Some(target) = self.ui_ephemeral.clip_edit_buffer_target {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        self.ui_ephemeral.status_message =
            format!("Auto-Crossfade: {applied} ペアの境界にクロスフェードを掛けました");
    }
}
