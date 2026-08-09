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
    pub(crate) fn open_audio_editor(&mut self, target: ClipRef) {
        if !self.is_audio_clip(target) {
            return;
        }
        // 別 clip を開くときは前 clip の選択 index は stale なので clear
        // (同 clip の再 open は選択を保持)。 index ベース選択は context が
        // 変わると意味を失う (= close / undo と同方針)。
        if self.ui_ephemeral.audio_editor_clip != Some(target) {
            self.selection.audio_editor_selected_events.clear();
        }
        self.ui_ephemeral.audio_editor_clip = Some(target);
        self.ui_prefs.bottom_panel = 1;
        // per-clip 記憶。 初回 (entry 無し) のクリップだけ「全体表示」の初期 view を
        // 入れる。 既に記憶があればその view を復元 (= map をそのまま読む)。
        let Some(key) = self.clip_key_of(target) else { return };
        if !self.ui_prefs.audio_editor_views.contains_key(&key) {
            let len_beats = self
                .song_doc.song()
                .tracks
                .get(target.track as usize)
                .and_then(|t| t.clips.get(target.clip as usize))
                .map_or(0.0, |c| c.length_beats);
            self.ui_prefs.audio_editor_views.insert(
                key,
                common::model::AudioEditorViewState {
                    start_beat: 0.0,
                    len_beats: len_beats.max(0.0),
                },
            );
        }
    }

    pub(crate) fn close_audio_editor(&mut self) {
        // view 状態は `audio_editor_views` に残す (= 次回 open で復元)。
        self.ui_ephemeral.audio_editor_clip = None;
        self.selection.audio_editor_selected_events.clear();
        self.ui_ephemeral.audio_editor_hover_beat_in_clip = None;
    }

    /// Audio Editor 水平 scroll: `view_start_beat` を `[0, total - view_len]`
    /// で clamp。 `audio_editor_clip` が None / clip が解決できない場合は no-op。
    pub(crate) fn set_audio_editor_scroll(&mut self, new_start: f64) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else { return };
        let Some(key) = self.clip_key_of(target) else { return };
        let Some(total) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.length_beats.max(0.0))
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
        let max_start = (total - view_len).max(0.0);
        self.ui_prefs.audio_editor_views.entry(key).or_default().start_beat = new_start.clamp(0.0, max_start);
    }

    /// Audio Editor zoom: `view_start_beat` + `view_len_beats` を一括設定。
    /// `view_len` は `[MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, clip.length]`、
    /// `view_start` は `[0, clip.length - view_len]` で clamp。
    pub(crate) fn set_audio_editor_zoom(&mut self, new_start: f64, new_len: f64) {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else { return };
        let Some(key) = self.clip_key_of(target) else { return };
        let Some(total) = self
            .song_doc.song()
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.length_beats.max(0.0))
        else {
            return;
        };
        let len = new_len.clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total.max(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS));
        let max_start = (total - len).max(0.0);
        let entry = self.ui_prefs.audio_editor_views.entry(key).or_default();
        entry.start_beat = new_start.clamp(0.0, max_start);
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
            let track = song.tracks.get_mut(target.track as usize)?;
            let clip = track.clips.get_mut(target.clip as usize)?;
            let content_id = clip.content_id;
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
            if needed > clip.length_beats {
                clip.length_beats = needed;
            }
            Some(insert_at)
        }) else {
            return;
        };
        self.selection.audio_editor_selected_events = vec![insert_at];
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
        target: ClipRef,
        event_idx: usize,
        new_start_beats: f64,
    ) {
        let changed = self.edit_song_checked(|song| {
            let Some(track) = song.tracks.get_mut(target.track as usize) else {
                return false;
            };
            let Some(clip) = track.clips.get_mut(target.clip as usize) else {
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
            if needed > clip.length_beats {
                clip.length_beats = needed;
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
        target: ClipRef,
        event_idx: usize,
        side: AudioEventTrimSide,
        delta_beats: f64,
    ) {
        let bpm = self.song_doc.song().bpm.max(1.0) as f64;
        // source 情報を先に snapshot (= 後の mut borrow と分離)。
        let (sr_hz, total_frames) = {
            let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get(target.clip as usize) else {
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
            let Some(track) = song.tracks.get_mut(target.track as usize) else {
                return false;
            };
            let Some(clip) = track.clips.get_mut(target.clip as usize) else {
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
            if needed > clip.length_beats {
                clip.length_beats = needed;
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
        target: ClipRef,
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
            let track = song.tracks.get_mut(target.track as usize)?;
            let clip = track.clips.get_mut(target.clip as usize)?;
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Audio(audio)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return None;
            };
            let new_event = AudioEvent {
                source_id,
                event_start_in_clip_beats: position,
                event_length_beats: length_beats,
                source_start_frames: 0,
                source_end_frames,
                ..AudioEvent::default()
            };
            audio.events.push(new_event);
            let new_idx = audio.events.len() - 1;
            let needed = position + length_beats;
            if needed > clip.length_beats {
                clip.length_beats = needed;
            }
            Some(new_idx)
        }) else {
            return;
        };
        self.selection.audio_editor_selected_events = vec![new_idx];
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
        self.selection.audio_editor_selected_events = deduped;
        if !self.selection.audio_editor_selected_events.is_empty() {
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
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let mut indices: Vec<usize> = self.selection.audio_editor_selected_events.clone();
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
        self.selection.audio_editor_selected_events.clear();
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
        let targets: Vec<ClipRef> = if self.selection.selected_clips.is_empty() {
            self.selected_clip_ref().into_iter().collect()
        } else {
            self.selected_clip_refs()
        };
        for target in targets {
            let Some(content_id) = self
                .song_doc.song()
                .tracks
                .get(target.track as usize)
                .and_then(|t| t.clips.get(target.clip as usize))
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

    /// 隣接 audio clip ペアに crossfade を作成 (`docs/plan_audio_clip
    /// .md` §3.5 Auto-Crossfade)。 selected_clips のうち audio clip を
    /// track 別に集めて start_beat 順に並べ、 ペアごとに `prev_end >
    /// next_start` (= overlap 中) のみ overlap_beats を fade_out / fade_in
    /// に設定する。 隙間ペアは no-op、 完全重なり (next が prev に
    /// 内包される) はサポート対象外で skip + 警告。
    pub(crate) fn auto_crossfade_selected_clips(&mut self) {
        // (track_idx, clip_idx, start_beat, end_beat, content_id) を集める
        let mut entries: Vec<(u32, u32, f64, f64, u32)> = Vec::new();
        let targets: Vec<ClipRef> = if self.selection.selected_clips.is_empty() {
            self.selected_clip_ref().into_iter().collect()
        } else {
            self.selected_clip_refs()
        };
        for target in &targets {
            let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
                continue;
            };
            let Some(clip) = track.clips.get(target.clip as usize) else {
                continue;
            };
            let Some(common::model::ClipContent::Audio(_)) =
                self.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            entries.push((
                target.track,
                target.clip,
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
        // track ごとに sort して隣接ペアを抽出
        entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut applied = 0usize;
        for window in entries.windows(2) {
            let (prev_track, _, prev_start, prev_end, prev_content) = window[0];
            let (next_track, _, next_start, next_end, next_content) = window[1];
            if prev_track != next_track {
                continue;
            }
            if next_start >= prev_end {
                continue; // 隙間あり、 crossfade 対象外
            }
            if next_end <= prev_end {
                tracing::warn!(
                    prev_start, prev_end, next_start, next_end,
                    "Auto-Crossfade: next clip が prev に内包されているため skip"
                );
                continue;
            }
            let overlap = (prev_end - next_start).max(0.0);
            self.edit_song(|song| {
                // prev clip の末尾 fade_out
                if let Some(common::model::ClipContent::Audio(audio)) =
                    song.clip_contents.get_mut(&prev_content)
                {
                    for event in &mut audio.events {
                        event.fade_out_beats = overlap.min(event.event_length_beats);
                    }
                }
                // next clip の先頭 fade_in
                if let Some(common::model::ClipContent::Audio(audio)) =
                    song.clip_contents.get_mut(&next_content)
                {
                    for event in &mut audio.events {
                        event.fade_in_beats = overlap.min(event.event_length_beats);
                    }
                }
            });
            applied += 1;
        }
        if applied > 0 {
            if let Some(target) = self.ui_ephemeral.clip_edit_buffer_target {
                self.resync_clip_audio_event_edit_buffers(target);
            }
            self.ui_ephemeral.status_message =
                format!("Auto-Crossfade: {applied} ペアに crossfade を適用");
        } else {
            self.ui_ephemeral.status_message =
                "Auto-Crossfade: 重なっている隣接ペアがありません".into();
        }
    }

}
