//! handler::clips — clip の resize/stretch/duplicate/create/delete/split
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{AudioContent, AudioEvent, Clip, ClipContent, MidiContent, Note};

impl AppData {
    /// Clip の左右端 trim ハンドラ。 caller (arrangement widget) は
    /// `ResizeClipDelta { prev_start, next_start, prev_len, next_len }`
    /// から `next_start` / `next_len` を直接渡す。 ここで `delta_start =
    /// new_start_beat - prev_start_beat` を計算し、 audio clip では
    /// 各 event の clip 内位置 (`event_start_in_clip_beats`) と source 切り
    /// 出し (`source_start_frames` / `event_length_beats`) を整合させる
    /// (Bitwig 流 §3.2)。 MIDI clip では既存どおり `start_beat` /
    /// `length_beats` のみ更新。
    ///
    /// 左端 trim (delta_start > 0):
    /// - clip.start_beat += delta_start、 clip.length_beats -= delta_start (= next_len)
    /// - 各 event: clip 内 beats 軸を維持するため event_start_in_clip_beats
    ///   から delta_start を引く。 event の絶対位置 (= clip.start_beat +
    ///   event.event_start_in_clip_beats) は変わらない (= source の同位置を
    ///   そのまま再生する)
    /// - delta_start が event の途中に入った場合は event の左端を切り
    ///   詰める: event_start_in_clip_beats = 0、 event_length_beats を
    ///   削った分だけ縮める、 source_start_frames を delta_samples 進める
    ///
    /// 左端を伸ばす (delta_start < 0): event は単に右へスライド (= source
    /// は変えない、 clip 先頭の追加範囲は無音)。 source_start_frames を
    /// 負方向に動かすのは安全でない (source 開始フレームを超えると
    /// 配列範囲外) ので、 単純な後方スライドのみ。
    ///
    /// 右端 trim (delta_start == 0): length_beats を変え、 audio event は
    /// `source_end_frames` を event 長に **lockstep** させる (旧実装は
    /// event 長を clamp するだけで source 窓を動かさず、 波形が clip 幅に
    /// rubber-band されて「見た目だけ伸縮・音は range」 という矛盾になっていた)。
    ///
    /// `stretch == true` (Shift + 端 drag) は trim ではなく
    /// **time-stretch** (= 内容を新 clip 長に伸縮)。 `stretch_clip_content` 参照。
    pub(crate) fn resize_clip(
        &mut self,
        target: ClipRef,
        new_start_beat: f64,
        new_length_beats: f64,
        stretch: bool,
    ) {
        let new_length_beats = new_length_beats.max(0.0625);
        let new_start_beat = new_start_beat.max(0.0);
        let bpm = self.song_doc.song().bpm.max(1.0) as f64;
        let Some(Some((content_id, prev_start_beat, prev_length_beats))) =
            self.edit_song(|song| {
                let track = song.tracks.get_mut(target.track as usize)?;
                let clip = track.clips.get_mut(target.clip as usize)?;
                let prev_start_beat = clip.start_beat;
                let prev_length_beats = clip.length_beats;
                clip.start_beat = new_start_beat;
                clip.length_beats = new_length_beats;
                Some((clip.content_id, prev_start_beat, prev_length_beats))
            })
        else {
            return;
        };
        let delta_start = new_start_beat - prev_start_beat;

        // Shift + 端 drag = time-stretch。 content を新 clip 長に
        // 伸縮し (audio は source 窓固定で event 長変更 + Raw→Stretch 昇格、
        // MIDI は note を比例 scale)、 trim とは別経路で処理する。
        if stretch {
            self.stretch_clip_content(
                target,
                content_id,
                prev_start_beat,
                prev_length_beats,
                new_start_beat,
                new_length_beats,
            );
            return;
        }

        // ---- trim (= 再生範囲を変える) ----
        // Snapshot の per-source metadata (event ごとに lookup できるよう
        // immutable borrow を先に切る)。
        let audio_sources = self.song_doc.song().media.audio_sources.clone();
        self.edit_song(|song| {
            if let Some(ClipContent::Audio(audio)) = song.clip_contents.get_mut(&content_id) {
                for event in &mut audio.events {
                    Self::trim_audio_event(
                        event,
                        delta_start,
                        prev_length_beats,
                        new_length_beats,
                        bpm,
                        &audio_sources,
                    );
                }
            }
        });

        // overlay clip (image / video / text) は「clip 長 = 表示長」が
        // 不変条件。 Audio/Midi では no-op、 overlay の末尾 event だけ新 clip 長
        // まで extend する (extend-only / idempotent / linked clip 安全)。
        self.edit_song(|song| {
            if let Some(content) = song.clip_contents.get_mut(&content_id) {
                content.ensure_event_covers_clip(new_length_beats);
            }
        });

    }

    /// trim (= 再生範囲を変える) の 1 audio event 分の追従。 source 窓
    /// (`source_start/end_frames`) と event 長 (`event_length_beats`) を
    /// **lockstep** させる (= 現在の frames-per-beat 比を保ったまま窓を動かす)。
    /// これで (a) 右端を縮めると source_end も縮んで波形が crop 表示になり、
    /// (b) 左端の出し入れで source_start が往復し、 「波形は伸縮するのに音は
    /// range だけ変わる」 という #61 の矛盾が解消する (stretch = 比を変える、 とは
    /// 別物)。 比は event の現値から取るので Raw でも stretch 済 event でも正しい。
    pub(crate) fn trim_audio_event(
        event: &mut AudioEvent,
        delta_start: f64,
        prev_length_beats: f64,
        new_length_beats: f64,
        bpm: f64,
        sources: &std::collections::HashMap<common::model::AudioSourceId, common::model::AudioSource>,
    ) {
        let source = sources.get(&event.source_id);
        let source_frames = source.map_or(u64::MAX, |s| s.frames);
        let source_sr = source.map_or(48_000.0, |s| f64::from(s.sample_rate));
        // 現在の source 窓 / event 長 = frames-per-beat (= trim で保つ比)。
        // 退化 (0 長 / 0 窓) は native (Raw) rate に fallback。
        let orig_len = event.event_length_beats;
        let orig_span = event
            .source_end_frames
            .saturating_sub(event.source_start_frames);
        let fpb = if orig_len > 1e-9 && orig_span > 0 {
            orig_span as f64 / orig_len
        } else {
            source_sr * 60.0 / bpm
        }
        .max(1e-9);
        // この event が clip 右端まで届いているか (= clip の右境界を所有するか)。
        // 多 event clip で、 clip を伸ばしたとき「右端を所有する event だけ」 を
        // 伸ばし、 中間 event は長さ据え置き (clip を縮めたときの cut は両者共通)。
        let reached_end = event.event_start_in_clip_beats + orig_len >= prev_length_beats - 1e-6;

        // --- 左端 ---
        if delta_start > 0.0 {
            // 左端を右へ: 絶対位置維持で event_start を手前に。 越えたら head chop。
            let new_evt_start = event.event_start_in_clip_beats - delta_start;
            if new_evt_start >= 0.0 {
                event.event_start_in_clip_beats = new_evt_start;
            } else {
                let chopped = -new_evt_start;
                let chopped_frames = (chopped * fpb).max(0.0) as u64;
                event.event_start_in_clip_beats = 0.0;
                event.source_start_frames = event
                    .source_start_frames
                    .saturating_add(chopped_frames)
                    .min(event.source_end_frames);
            }
        } else if delta_start < 0.0 {
            if event.event_start_in_clip_beats <= 1e-9 {
                // 左端を左へ (spanning event): source head を再露出 (source_start
                // を戻す)、 source 先頭で頭打ち。 足りない分は無音前置きとして
                // event をスライド (源より手前は無音)。
                let reveal = -delta_start;
                let reveal_frames = (reveal * fpb).max(0.0) as u64;
                let actual_frames = reveal_frames.min(event.source_start_frames);
                event.source_start_frames -= actual_frames;
                let remainder = reveal - actual_frames as f64 / fpb;
                if remainder > 1e-9 {
                    event.event_start_in_clip_beats += remainder;
                }
            } else {
                // 前方タイル event は単純後方スライド (source 不変)。
                event.event_start_in_clip_beats -= delta_start;
            }
        }

        // --- 右端: source_end を event 長に lockstep ---
        // 右端を所有する event は clip 長まで充填 (grow/shrink)、 中間 event は
        // 長さ据え置き (ただし clip を縮めたら cut)。 いずれも source_end は
        // 結果長に lockstep するので波形は crop 表示になる (#61)。
        let max_event_len = (new_length_beats - event.event_start_in_clip_beats).max(0.0);
        let avail_beats =
            source_frames.saturating_sub(event.source_start_frames) as f64 / fpb;
        let desired_len = if reached_end {
            max_event_len
        } else {
            orig_len.min(max_event_len)
        };
        let target_len = desired_len.min(avail_beats);
        event.event_length_beats = target_len;
        let span_frames = (target_len * fpb).max(0.0) as u64;
        event.source_end_frames = event
            .source_start_frames
            .saturating_add(span_frames)
            .min(source_frames);
    }

    /// Shift + 端 drag = time-stretch。 clip 内容を新 clip 長に伸縮する。
    /// audio は source 窓 (`source_start/end_frames`) を **固定**して event 長のみ
    /// 変え (engine が `stretch_ratio = native/event 長` で warp 再生)、 Raw は
    /// pitch 保持の `Stretch` (granular) へ昇格 (= ピッチ保持が既定)。 MIDI は
    /// note の `start_beat` / `duration_beats` を比例 scale。 共有 content は fork
    /// してから伸縮し linked siblings (= 別 length) を巻き込まない。 pivot は
    /// 固定端 (右端 drag = 左端固定 / 左端 drag = 右端固定)。
    pub(crate) fn stretch_clip_content(
        &mut self,
        target: ClipRef,
        content_id: common::model::ContentId,
        prev_start: f64,
        prev_len: f64,
        new_start: f64,
        new_len: f64,
    ) {
        if prev_len <= 1e-9 || new_len <= 1e-9 {
            return;
        }
        // 共有 content は fork してから伸縮 (siblings の length と無関係)。
        let content_id = if self.song_doc.song().clip_content_refcount(content_id) > 1 {
            self.edit_song(|song| {
                let new_id = song.fork_content(content_id);
                if let Some(clip) = song
                    .tracks
                    .get_mut(target.track as usize)
                    .and_then(|t| t.clips.get_mut(target.clip as usize))
                {
                    clip.content_id = new_id;
                }
                new_id
            })
            .unwrap_or(content_id)
        } else {
            content_id
        };

        self.edit_song(|song| {
            match song.clip_contents.get_mut(&content_id) {
                Some(ClipContent::Audio(audio)) => {
                    for e in &mut audio.events {
                        let (s, l) = stretch_remap(
                            prev_start,
                            prev_len,
                            new_start,
                            new_len,
                            e.event_start_in_clip_beats,
                            e.event_length_beats,
                        );
                        e.event_start_in_clip_beats = s;
                        e.event_length_beats = l;
                        // ピッチ保持を既定: Raw (= 時間操作しない定義) は Stretch
                        // (granular) へ昇格。 既に Repitch/Stretch/Slice なら維持。
                        if e.stretch_mode == common::model::StretchMode::Raw {
                            e.stretch_mode = common::model::StretchMode::Stretch;
                        }
                        // source 窓は固定 = これが stretch の本質。
                    }
                }
                Some(ClipContent::Midi(midi)) => {
                    for n in &mut midi.notes {
                        let (s, l) = stretch_remap(
                            prev_start,
                            prev_len,
                            new_start,
                            new_len,
                            n.start_beat,
                            n.duration_beats,
                        );
                        n.start_beat = s;
                        n.duration_beats = l;
                    }
                }
                other => {
                    // overlay / automation は stretch 概念なし → 長さ追従のみ。
                    if let Some(content) = other {
                        content.ensure_event_covers_clip(new_len);
                    }
                }
            }
        });
    }

    /// 共有コピー (D shortcut): 末尾直後 (start+length) に同サイズの clip を
    /// 1 つ生成、 `content_id` を流用。 `docs/plan_clip_share_clone.md` §3.2。
    /// 選択 clip 群の bounding span (`max_end - min_start`)。 複製を選択ブロック
    /// 直後に並べるためのオフセット (相対位置を保ったままブロック複製)。 単一
    /// clip では clip 長と一致する (= 旧 single duplicate と同挙動)。 解決でき
    /// ない stale ref は無視、 有効 clip が 1 つも無ければ `None`。
    pub(crate) fn clip_block_span(&self, sources: &[ClipRef]) -> Option<f64> {
        let mut min_start = f64::MAX;
        let mut max_end = f64::MIN;
        for &src in sources {
            let Some(clip) = self
                .song_doc.song()
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
            else {
                continue;
            };
            min_start = min_start.min(clip.start_beat);
            max_end = max_end.max(clip.start_beat + clip.length_beats);
        }
        (max_end >= min_start).then_some(max_end - min_start)
    }

    /// `source` の共有コピーを `new_start_beat` に 1 つ生成し、 新 `ClipRef` を
    /// 返す (選択・sync は呼び出し側)。 同 `content_id` を流用 → 名前 (content_id
    /// 単位 SSoT) も共有、 色 (per-clip) は source 引き継ぎ。
    pub(crate) fn duplicate_one_clip_shared_at(
        &mut self,
        source: ClipRef,
        new_start_beat: f64,
    ) -> Option<ClipRef> {
        let src_clip = self
            .song_doc.song()
            .tracks
            .get(source.track as usize)?
            .clips
            .get(source.clip as usize)?;
        let new_length = src_clip.length_beats;
        let content_id = src_clip.content_id;
        let src_color = src_clip.color;
        // mute 状態も複製先へ引き継ぐ (color / 声 と同様)。
        let src_muted = src_clip.muted;
        // per-clip 声を複製先へ引き継ぐ。
        let src_speaker = src_clip.speaker_id;
        let src_singer = src_clip.singer_name.clone();
        let src_style = src_clip.style_name.clone();
        let src_talk = src_clip.talk;
        let new_idx = self.edit_song(move |song| {
            let track = song.tracks.get_mut(source.track as usize)?;
            let new_clip_id = track.alloc_clip_id();
            let new_idx = track.clips.len() as u32;
            track.clips.push(Clip {
                id: new_clip_id,
                start_beat: new_start_beat,
                length_beats: new_length,
                content_id,
                color: src_color,
                auto_lipsync: false,
                muted: src_muted,
                speaker_id: src_speaker,
                singer_name: src_singer,
                style_name: src_style,
                talk: src_talk,
            });
            Some(new_idx)
        })??;
        Some(ClipRef { track: source.track, clip: new_idx })
    }

    /// `source` の独立コピー (content を deep clone + 新 ContentId 採番) を
    /// `new_start_beat` に 1 つ生成し、 新 `ClipRef` を返す。 §3.3。
    pub(crate) fn duplicate_one_clip_unique_at(
        &mut self,
        source: ClipRef,
        new_start_beat: f64,
    ) -> Option<ClipRef> {
        let src_clip = self
            .song_doc.song()
            .tracks
            .get(source.track as usize)?
            .clips
            .get(source.clip as usize)?;
        let new_length = src_clip.length_beats;
        let src_content_id = src_clip.content_id;
        let src_color = src_clip.color;
        // mute 状態も複製先へ引き継ぐ。
        let src_muted = src_clip.muted;
        // per-clip 声を複製先へ引き継ぐ。
        let src_speaker = src_clip.speaker_id;
        let src_singer = src_clip.singer_name.clone();
        let src_style = src_clip.style_name.clone();
        let src_talk = src_clip.talk;
        let new_idx = self.edit_song(move |song| {
            let new_content_id = song.fork_content(src_content_id);
            let track = song.tracks.get_mut(source.track as usize)?;
            let new_clip_id = track.alloc_clip_id();
            let new_idx = track.clips.len() as u32;
            track.clips.push(Clip {
                id: new_clip_id,
                start_beat: new_start_beat,
                length_beats: new_length,
                content_id: new_content_id,
                color: src_color,
                auto_lipsync: false,
                muted: src_muted,
                speaker_id: src_speaker,
                singer_name: src_singer,
                style_name: src_style,
                talk: src_talk,
            });
            Some(new_idx)
        })??;
        Some(ClipRef { track: source.track, clip: new_idx })
    }

    /// 選択 clip 群をまとめて共有複製 (D shortcut)。 選択ブロック span
    /// だけ後ろにずらして相対位置を保ったまま複製し (Ctrl+drag と同じセマンティ
    /// クス)、 複製群を選択にする。 D 連打で後方連鎖する。
    pub(crate) fn duplicate_clips_shared(&mut self, sources: &[ClipRef]) {
        let Some(offset) = self.clip_block_span(sources) else {
            return;
        };
        let mut new_refs = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song_doc.song()
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(r) = self.duplicate_one_clip_shared_at(src, new_start) {
                new_refs.push(r);
            }
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selection.selected_notes.clear();
        }
    }

    /// 選択 clip 群をまとめて独立複製 (Alt+D shortcut)。 配置・選択は
    /// `duplicate_clips_shared` と同じ、 各 clip の content を独立化する点が違う。
    pub(crate) fn duplicate_clips_unique(&mut self, sources: &[ClipRef]) {
        let Some(offset) = self.clip_block_span(sources) else {
            return;
        };
        let mut new_refs = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song_doc.song()
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(r) = self.duplicate_one_clip_unique_at(src, new_start) {
                new_refs.push(r);
            }
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selection.selected_notes.clear();
        }
    }

    /// arrangement Ctrl+drag → release: 各 (source, drop_start_beat) で
    /// 共有コピーを生成。 元 clip 群はそのまま、 selected_clips は新 clip
    /// 群に置き換える (drag 後に選択が新 clip に移るのは MoveClips と同じ semantics)。
    /// §3.4。
    pub(crate) fn clone_clips_linked(&mut self, entries: &[(ClipRef, u32, f64)]) {
        let Some(new_refs) = self.edit_song(|song| {
            let mut new_refs = Vec::with_capacity(entries.len());
            for &(source, to_track_id, drop_start) in entries {
                let Some(track) = song.tracks.get(source.track as usize) else {
                    continue;
                };
                let Some(src_clip) = track.clips.get(source.clip as usize) else {
                    continue;
                };
                let new_length = src_clip.length_beats;
                // 共有コピー: content_id 流用 → 名前も自動共有。色 (per-clip) は
                // source の色を引き継ぐ。
                let content_id = src_clip.content_id;
                let src_color = src_clip.color;
                // mute 状態も複製先へ引き継ぐ。
                let src_muted = src_clip.muted;
                // per-clip 声を複製先へ引き継ぐ。
                let src_voice = (
                    src_clip.speaker_id,
                    src_clip.singer_name.clone(),
                    src_clip.style_name.clone(),
                    src_clip.talk,
                );
                let Some(to_track_idx) = song.track_index_by_id(to_track_id) else {
                    continue;
                };
                let Some(to_track) = song.tracks.get_mut(to_track_idx) else {
                    continue;
                };
                let new_clip_id = to_track.alloc_clip_id();
                let new_idx = to_track.clips.len() as u32;
                to_track.clips.push(Clip {
                    id: new_clip_id,
                    start_beat: drop_start.max(0.0),
                    length_beats: new_length,
                    content_id,
                    color: src_color,
                    auto_lipsync: false,
                    muted: src_muted,
                    speaker_id: src_voice.0,
                    singer_name: src_voice.1,
                    style_name: src_voice.2,
                    talk: src_voice.3,
                });
                new_refs.push(ClipRef {
                    track: to_track_idx as u32,
                    clip: new_idx,
                });
            }
            new_refs
        }) else {
            return;
        };
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selection.selected_notes.clear();
        }
    }

    /// arrangement Ctrl+Shift+drag → release: 各 (source, drop_start_beat)
    /// で独立コピーを生成。 §3.5。
    pub(crate) fn clone_clips_independent(&mut self, entries: &[(ClipRef, u32, f64)]) {
        let Some(new_refs) = self.edit_song(|song| {
            let mut new_refs = Vec::with_capacity(entries.len());
            for &(source, to_track_id, drop_start) in entries {
                let Some(track) = song.tracks.get(source.track as usize) else {
                    continue;
                };
                let Some(src_clip) = track.clips.get(source.clip as usize) else {
                    continue;
                };
                let new_length = src_clip.length_beats;
                // 独立コピー: content + 名前を fork。色 (per-clip) は source の色を引き継ぐ。
                let src_content_id = src_clip.content_id;
                let src_color = src_clip.color;
                // mute 状態も複製先へ引き継ぐ。
                let src_muted = src_clip.muted;
                // per-clip 声を複製先へ引き継ぐ。
                let src_voice = (
                    src_clip.speaker_id,
                    src_clip.singer_name.clone(),
                    src_clip.style_name.clone(),
                    src_clip.talk,
                );
                let new_content_id = song.fork_content(src_content_id);
                let Some(to_track_idx) = song.track_index_by_id(to_track_id) else {
                    continue;
                };
                let Some(to_track) = song.tracks.get_mut(to_track_idx) else {
                    continue;
                };
                let new_clip_id = to_track.alloc_clip_id();
                let new_idx = to_track.clips.len() as u32;
                to_track.clips.push(Clip {
                    id: new_clip_id,
                    start_beat: drop_start.max(0.0),
                    length_beats: new_length,
                    content_id: new_content_id,
                    color: src_color,
                    auto_lipsync: false,
                    muted: src_muted,
                    speaker_id: src_voice.0,
                    singer_name: src_voice.1,
                    style_name: src_voice.2,
                    talk: src_voice.3,
                });
                new_refs.push(ClipRef {
                    track: to_track_idx as u32,
                    clip: new_idx,
                });
            }
            new_refs
        }) else {
            return;
        };
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selection.selected_notes.clear();
        }
    }

    /// Make Unique (右クリック): 共有 clip → 独立化。 refcount==1 なら no-op。
    /// §3.6。
    pub(crate) fn make_clip_unique(&mut self, target: ClipRef) {
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        if self.song_doc.song().clip_content_refcount(content_id) <= 1 {
            self.ui_ephemeral.status_message = "すでに独立 clip です".to_string();
            return;
        }
        // content + 名前を fork して独立化 (fork 時点の名前を引き継ぐ)。
        let done = self
            .edit_song(|song| {
                let new_content_id = song.fork_content(content_id);
                if let Some(clip) = song
                    .tracks
                    .get_mut(target.track as usize)
                    .and_then(|t| t.clips.get_mut(target.clip as usize))
                {
                    clip.content_id = new_content_id;
                }
            })
            .is_some();
        if done {
            self.ui_ephemeral.status_message = "Clip を独立化しました".to_string();
        }
    }

    pub(crate) fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        let Some(Some(r)) = self.edit_song(|song| {
            // Allocate the shared content slot first so the new clip points
            // at a real entry. Orphan content_ids (if track lookup below
            // fails) get reclaimed by `Song::gc_clip_contents` before save.
            let content_id = song.alloc_content_id();
            song.clip_contents.insert(content_id, ClipContent::default());
            let track = song.tracks.get_mut(track_idx as usize)?;
            let new_clip_id = track.alloc_clip_id();
            let new_idx = track.clips.len() as u32;
            // vocal track の新規 clip は声を引き継ぐ: 同トラックの
            // 直前 (= start_beat 最大の既存) clip の声、 無ければアプリ既定
            // (中国うさぎ ノーマル)。 非 vocal track では声は未設定 (0)。
            let (speaker_id, singer_name, style_name) =
                if track.is_voicevox_vocal() {
                    track
                        .clips
                        .iter()
                        .filter(|c| c.speaker_id != 0)
                        .max_by(|a, b| a.start_beat.total_cmp(&b.start_beat))
                        .map(|c| (c.speaker_id, c.singer_name.clone(), c.style_name.clone()))
                        .unwrap_or_else(|| {
                            (
                                common::voicevox::DEFAULT_SINGER_ID,
                                common::voicevox::DEFAULT_SINGER_NAME.to_string(),
                                common::voicevox::DEFAULT_STYLE_NAME.to_string(),
                            )
                        })
                } else {
                    (0, String::new(), String::new())
                };
            track.clips.push(Clip {
                id: new_clip_id,
                start_beat,
                length_beats: DEFAULT_CLIP_LENGTH,
                content_id,
                color: None,
                auto_lipsync: false,
                muted: false,
                speaker_id,
                singer_name,
                style_name,
                // (talk) 新規 clip は読み上げスケール未設定 (= 全既定)。
                talk: None,
            });
            // デフォルトでクリップ名は無し (= content_name 未設定)。 表示名は
            // arrangement_view::clip_display_label が内容 (Text 本文 / ノート歌詞)
            // から導出する。 ユーザーが Rename したときだけ明示名が入る。
            Some(ClipRef {
                track: track_idx,
                clip: new_idx,
            })
        }) else {
            return;
        };
        self.set_single_clip_selection(r);
        self.selection.selected_notes.clear();
        self.select_track(track_idx);
    }

    pub(crate) fn delete_selected_clip(&mut self) {
        if self.selection.selected_clips.is_empty() {
            return;
        }
        // ClipKey → 現在の index ClipRef に解決し、 同 track 内は高 clip index
        // から remove して shift を回避する。
        let mut targets: Vec<ClipRef> = self.selected_clip_refs();
        self.selection.selected_clips.clear();
        targets.sort_by(|a, b| a.track.cmp(&b.track).then(b.clip.cmp(&a.clip)));
        self.edit_song(|song| {
            for target in &targets {
                if let Some(track) = song.tracks.get_mut(target.track as usize)
                    && (target.clip as usize) < track.clips.len()
                {
                    track.clips.remove(target.clip as usize);
                }
            }
        });
        self.selection.selected_clip = None;
        self.selection.selected_notes.clear();
    }

    /// Split clip(s) at the cursor (= mouse hover beat).
    ///
    /// If `snap` is `true`, uses the snapped beat; otherwise the raw
    /// beat (for `Alt+E` snap-temporarily-off flow). Falls back to the
    /// playhead when the cursor is outside the canvas. Targets are:
    ///
    /// 1. The clip the cursor is hovering over
    ///    (`arrangement_hover_clip`).
    /// 2. If no hover, the current `selected_clips` (multi-clip split
    ///    at the same beat).
    /// 3. If neither, surfaces a status message.
    ///
    /// The back half of each split clip receives a fresh `ContentId`
    /// (= leaves any share group, Make Unique-equivalent semantics).
    /// Works on MIDI / Audio / Vocal clips alike. See
    /// `docs/plan_audio_clip.md` §3.3 / §3.3.1.
    pub(crate) fn action_split_clips_at_cursor(&mut self, snap: bool) {
        // Audio Editor が開いていて、 マウスが waveform 領域内にある
        // ときは「audio_editor_clip を audio editor のマウス hover 位置
        // で split」 として優先処理する。 audio editor は bottom_panel
        // 内なので arrangement_hover_beat は更新されず、 既存 path だと
        // 「マウスを arrangement に置いて...」 status で no-op になる。
        // Audio Editor 上の波形領域に **マウスが乗っているとき** だけ
        // event 分割に振り分ける。 Audio Editor が開いていてもマウスが
        // arrangement 上にある場合は通常の clip 分割パスを使う (= ユーザー
        // は arrangement の clip を分割したいのでそのまま流す)。
        if self.ui_ephemeral.audio_editor_clip.is_some()
            && self.ui_ephemeral.audio_editor_hover_beat_in_clip.is_some()
        {
            self.action_split_audio_editor_event_at_cursor();
            return;
        }

        let cursor: f64 = if snap {
            self.ui_ephemeral.arrangement_hover_beat
                .or(self.ui_ephemeral.arrangement_hover_beat_raw)
                .or_else(|| self.transport.playhead_beat.map(|b| b as f64))
                .unwrap_or(-1.0)
        } else {
            self.ui_ephemeral.arrangement_hover_beat_raw
                .or(self.ui_ephemeral.arrangement_hover_beat)
                .or_else(|| self.transport.playhead_beat.map(|b| b as f64))
                .unwrap_or(-1.0)
        };
        if cursor < 0.0 {
            self.ui_ephemeral.status_message =
                "Split: マウスを arrangement に置くか再生中に E を押してください".into();
            return;
        }
        // Build targets list. Prefer hover clip, fall back to selection.
        let targets: Vec<ClipRef> = if let Some(hover) = self.ui_ephemeral.arrangement_hover_clip {
            vec![hover]
        } else if !self.selection.selected_clips.is_empty() {
            self.selected_clip_refs()
        } else {
            self.ui_ephemeral.status_message =
                "Split: clip にマウスを乗せるか clip を選択してください".into();
            return;
        };
        let mut split_count = 0usize;
        let mut new_selection: Vec<ClipRef> = Vec::new();
        for src in &targets {
            if self.split_clip_at_beat(*src, cursor, &mut new_selection) {
                split_count += 1;
            }
        }
        if split_count == 0 {
            self.ui_ephemeral.status_message =
                "Split: カーソルが clip 範囲外のため何も分割されませんでした".into();
            return;
        }
        if !new_selection.is_empty() {
            self.select_new_clips(&new_selection);
            self.selection.selected_notes.clear();
        }
        self.ui_ephemeral.status_message = format!("Split: {split_count} clip を分割しました");
    }

    /// Audio Editor が開いているとき、 cursor 位置 (= マウス hover、
    /// fallback で playhead) が乗っている event を 2 つに分割する。
    /// `audio_editor_clip` は変更せず、 audio content の events Vec に
    /// 後半 event を `event_idx + 1` の位置に挿入。 fade_out (前半側) と
    /// fade_in (後半側) は 0 にリセット (Bitwig / Reaper の split 慣行)。
    /// 選択は後半 event に移動。
    ///
    /// 戻り値は分割成功時 `true`。 cursor が解決できない / event 上に
    /// 乗っていない場合は status_message を出して `false` を返す。
    pub(crate) fn action_split_audio_editor_event_at_cursor(&mut self) -> bool {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return false;
        };

        // cursor 位置 (clip 内 beat)。 hover (= マウスが waveform 上)
        // を最優先、 無ければ playhead が clip 内なら playhead を使う。
        let in_clip_beat: Option<f64> = self
            .ui_ephemeral.audio_editor_hover_beat_in_clip
            .or_else(|| {
                let ph = self.transport.playhead_beat? as f64;
                let clip = self
                    .song_doc.song()
                    .tracks
                    .get(target.track as usize)?
                    .clips
                    .get(target.clip as usize)?;
                let in_clip = ph - clip.start_beat;
                (in_clip >= 0.0 && in_clip < clip.length_beats).then_some(in_clip)
            });
        let Some(in_clip_beat) = in_clip_beat else {
            self.ui_ephemeral.status_message =
                "Split: マウスを Audio Editor の波形上に置くか playhead を clip 内に置いてください"
                    .into();
            return false;
        };

        // event_idx を解決 (= cursor が strict interior に乗っている event)。
        let track = self
            .song_doc.song()
            .tracks
            .get(target.track as usize);
        let clip = track.and_then(|t| t.clips.get(target.clip as usize));
        let Some(clip) = clip else { return false };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio_ro)) =
            self.song_doc.song().clip_contents.get(&content_id)
        else {
            return false;
        };
        let event_idx_opt = audio_ro.events.iter().position(|e| {
            let s = e.event_start_in_clip_beats;
            let l = e.event_length_beats;
            in_clip_beat > s + 1e-9 && in_clip_beat < s + l - 1e-9
        });
        let Some(event_idx) = event_idx_opt else {
            self.ui_ephemeral.status_message =
                "Split: カーソル位置に分割可能な event がありません".into();
            return false;
        };
        // 元 event を clone して詳細パラメータを後半 event にコピー。
        let event = audio_ro.events[event_idx].clone();

        let offset_in_event = in_clip_beat - event.event_start_in_clip_beats;
        let len_beats = event.event_length_beats.max(1e-9);
        let event_len_frames = event
            .source_end_frames
            .saturating_sub(event.source_start_frames);
        let frame_offset = ((offset_in_event / len_beats) * event_len_frames as f64)
            .round()
            .clamp(0.0, event_len_frames as f64) as u64;

        // reversed のときは clip 時間 → source frame の対応が逆向き
        // (event_start に source_end が、 event_end に source_start が
        // 対応)。 split frame も反転して計算する。
        let (front_ss, front_se, back_ss, back_se) = if event.reversed {
            let mid = event.source_end_frames.saturating_sub(frame_offset);
            (mid, event.source_end_frames, event.source_start_frames, mid)
        } else {
            let mid = event.source_start_frames + frame_offset;
            (event.source_start_frames, mid, mid, event.source_end_frames)
        };

        // 後半 event は元 event のパラメータ (gain / pan / pitch / fade /
        // stretch / reversed / muted / onsets / beat_markers) を引き継ぐ。
        // event_start は cursor 位置、 length は残り、 source は分割後の
        // 後半側、 fade_in は 0 にリセット (左端が新しいため)。
        let mut back = event.clone();
        back.source_start_frames = back_ss;
        back.source_end_frames = back_se;
        back.event_start_in_clip_beats = in_clip_beat;
        back.event_length_beats = (len_beats - offset_in_event).max(0.0);
        back.fade_in_beats = 0.0;
        // mut 取り直し → 分割実行。
        let inserted = self.edit_song_checked(|song| {
            let Some(common::model::ClipContent::Audio(audio_mut)) =
                song.clip_contents.get_mut(&content_id)
            else {
                return false;
            };
            // 前半 event を in-place で更新 (= event_start は変えず、 length と
            // source 範囲を縮める)。 fade_out は split で消す (右端が新しく
            // なったので元 fade_out 値は意味を失う)。
            {
                let front = &mut audio_mut.events[event_idx];
                front.source_start_frames = front_ss;
                front.source_end_frames = front_se;
                front.event_length_beats = offset_in_event;
                front.fade_out_beats = 0.0;
            }
            audio_mut.events.insert(event_idx + 1, back);
            true
        });
        if !inserted {
            return false;
        }

        // 選択は後半 event (= ユーザーは「分割直後に新規 event を編集
        // したい」 ことが多い、 Reaper / Bitwig 流)。
        self.selection.audio_editor_selected_events = vec![event_idx + 1];
        self.ui_ephemeral.status_message = "Split: event を分割しました".into();
        if self.ui_ephemeral.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        true
    }

    /// Single-clip split helper. Returns `true` iff the playhead lay
    /// strictly inside the clip and the split actually happened. The
    /// new (back-half) clip is appended to `new_selection` so the
    /// caller can update the selection afterwards.
    pub(crate) fn split_clip_at_beat(
        &mut self,
        target: ClipRef,
        playhead: f64,
        new_selection: &mut Vec<ClipRef>,
    ) -> bool {
        let Some(track) = self.song_doc.song().tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        let clip_start = clip.start_beat;
        let clip_len = clip.length_beats;
        let clip_end = clip_start + clip_len;
        // 色 (per-clip) は両半が引き継ぐ (= 色付き clip を split したら両方同色)。
        // front は clip_mut をそのまま使うので色は不変、 back の新 clip にこれを写す。
        let src_color = clip.color;
        if !(playhead > clip_start && playhead < clip_end) {
            return false; // playhead 範囲外 / 端ぴったりは split 不要
        }
        let split_offset = playhead - clip_start;
        let front_len = split_offset;
        let back_len = clip_len - split_offset;
        let src_content_id = clip.content_id;
        // 名前は content_id 単位 SSoT から取得 (legacy clip.name は v20 で空)。
        let src_name = self.song_doc.song().content_name(src_content_id).to_string();
        let Some(src_content) = self.song_doc.song().clip_contents.get(&src_content_id).cloned()
        else {
            return false;
        };

        // Build the back-half ClipContent by partitioning the source
        // content at `split_offset` (clip-local beats).
        self.edit_song_checked(|song| {
        let back_content = match src_content.clone() {
            ClipContent::Midi(mut midi) => {
                let mut back_notes: Vec<Note> = Vec::new();
                let mut keep_front: Vec<Note> = Vec::new();
                for note in midi.notes.drain(..) {
                    let n_start = note.start_beat;
                    let n_end = note.start_beat + note.duration_beats;
                    if n_end <= split_offset {
                        keep_front.push(note);
                    } else if n_start >= split_offset {
                        back_notes.push(Note {
                            start_beat: n_start - split_offset,
                            ..note
                        });
                    } else {
                        // Note straddles the split point — front half
                        // keeps lyric, back half is a continuation
                        // (no lyric so VOICEVOX doesn't sing it twice).
                        let front_dur = split_offset - n_start;
                        let back_dur = n_end - split_offset;
                        keep_front.push(Note {
                            start_beat: n_start,
                            duration_beats: front_dur,
                            ..note.clone()
                        });
                        back_notes.push(Note {
                            start_beat: 0.0,
                            duration_beats: back_dur,
                            lyric: None,
                            ..note
                        });
                    }
                }
                // Trim the original (front) content in place so the
                // share group keeps the front half only — but only
                // for THIS clip's content; if other clips share the
                // same `content_id` we must fork via a fresh id. We
                // always fork here for simplicity (= split always
                // promotes both halves to fresh ContentIds, which is
                // safer for shared-clip semantics).
                // v29: 両半とも元 content の allocator counter を引き継ぐ
                // (既存 note id はそのまま持ち出すので、 counter も既存 id を
                // カバーする値でなければ次の採番が衝突する)。
                let mut front = MidiContent {
                    notes: keep_front,
                    next_note_id: midi.next_note_id,
                };
                front.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                let mut back = MidiContent {
                    notes: back_notes,
                    next_note_id: midi.next_note_id,
                };
                back.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                let front_id = song.alloc_content_id();
                song.clip_contents
                    .insert(front_id, ClipContent::Midi(front));
                ClipContent::Midi(back)
            }
            ClipContent::Audio(mut audio) => {
                let mut back_events: Vec<AudioEvent> = Vec::new();
                let mut keep_front: Vec<AudioEvent> = Vec::new();
                for ev in audio.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(AudioEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        // Event straddles the split: split source range
                        // proportionally by the source-frame stride
                        // implied by this event's pitch_ratio is
                        // approximated as a simple linear partition
                        // (good enough for Phase 1 default Raw mode
                        // where source beats == clip beats × bpm).
                        let frac_front = (split_offset - e_start) / ev.event_length_beats;
                        let total_src = ev
                            .source_end_frames
                            .saturating_sub(ev.source_start_frames);
                        let split_src_offset =
                            (total_src as f64 * frac_front).round() as u64;
                        let mid_src_frame = ev.source_start_frames + split_src_offset;
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.source_end_frames = mid_src_frame;
                        keep_front.push(front_ev);
                        back_events.push(AudioEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            source_start_frames: mid_src_frame,
                            ..ev
                        });
                    }
                }
                // v29: split 両半は元 allocator counter を引き継ぐ (上の MIDI と同理)。
                let front = AudioContent {
                    events: keep_front,
                    next_event_id: audio.next_event_id,
                };
                let back = AudioContent {
                    events: back_events,
                    next_event_id: audio.next_event_id,
                };
                let front_id = song.alloc_content_id();
                song.clip_contents
                    .insert(front_id, ClipContent::Audio(front));
                ClipContent::Audio(back)
            }
            // Automation clips live on `Track.automation_lanes`, not in
            // `Track.clips`. Reaching here means the content store has
            // a stale Automation entry referenced from a MIDI/Audio
            // clip — refuse to split rather than guess.
            ClipContent::Automation(_) => return false,
            // Video clip split (docs/plan_video.md §4 P6). Mirrors the
            // Audio path: partition events front/back by split_offset,
            // straddling events get source_micros range proportionally
            // bisected (= linear partition; CFR assumption holds since
            // MVP doesn't expose time-stretch). Both halves allocate
            // fresh content_ids so the linked-clip semantics of the
            // source clip don't follow the split.
            ClipContent::Video(mut video) => {
                let mut back_events: Vec<common::model::VideoEvent> = Vec::new();
                let mut keep_front: Vec<common::model::VideoEvent> = Vec::new();
                for ev in video.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(common::model::VideoEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        let frac_front = (split_offset - e_start) / ev.event_length_beats;
                        let total_src =
                            ev.source_end_micros.saturating_sub(ev.source_start_micros);
                        let split_src_offset =
                            (total_src as f64 * frac_front).round() as u64;
                        let mid_src_micros = ev.source_start_micros + split_src_offset;
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.source_end_micros = mid_src_micros;
                        keep_front.push(front_ev);
                        back_events.push(common::model::VideoEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            source_start_micros: mid_src_micros,
                            ..ev
                        });
                    }
                }
                let front = common::model::VideoContent {
                    events: keep_front,
                };
                let back = common::model::VideoContent {
                    events: back_events,
                };
                let front_id = song.alloc_content_id();
                song.clip_contents
                    .insert(front_id, ClipContent::Video(front));
                ClipContent::Video(back)
            }
            // Image clip Split (`docs/plan_image_overlay.md` §4 P4)。
            // Audio / Video と同じ「event を split_offset で前後に振り分け」
            // pattern。 ImageEvent は単一画像 source への参照 + PiP rect /
            // opacity のみ持つので、 source 切り出し位置 (source_*_frames /
            // source_*_micros) のような時間軸 attribute は無い。 そのため
            // straddle event は時間長 (event_length_beats) だけを 2 つに
            // 分割し、 PiP rect / opacity / source_id は両 event が共有
            // (= 同じ画像を 2 つの時間 region で表示し続ける)。 fade_out
            // (前半側) / fade_in (後半側) は 0 にリセット (Audio / Video
            // と同じ split 慣行)。
            ClipContent::Image(mut image) => {
                let mut back_events: Vec<common::model::ImageEvent> = Vec::new();
                let mut keep_front: Vec<common::model::ImageEvent> = Vec::new();
                for ev in image.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(common::model::ImageEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.fade_out_beats = 0.0;
                        keep_front.push(front_ev);
                        back_events.push(common::model::ImageEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            fade_in_beats: 0.0,
                            ..ev
                        });
                    }
                }
                let front = common::model::ImageContent { events: keep_front };
                let back = common::model::ImageContent { events: back_events };
                let front_id = song.alloc_content_id();
                song.clip_contents
                    .insert(front_id, ClipContent::Image(front));
                ClipContent::Image(back)
            }
            // Text clip Split (`docs/plan_text_overlay.md` §2.2)。 image
            // split と同 idiom: event を split_offset で前後に振り分け。
            // text 内容 / font / color 等は両半が共有 (= 同 text の 2 つ
            // の時間 region で表示)、 fade_out (前半) / fade_in (後半) は
            // 0 リセット。 後 commit で実装、 まずは split skip で
            // build を通す。
            ClipContent::Text(_) => return false,
        };

        // Allocate fresh ContentIds for both halves (front was just
        // inserted into clip_contents above with a placeholder id —
        // we now rewrite the clip's content_id to point at it).
        // Strategy: walk back the last alloc'd id we just inserted.
        // The id list above used `alloc_content_id()` so the most
        // recent one is `next_content_id - 1`.
        let front_content_id = song.next_content_id.saturating_sub(1);
        let back_content_id = song.alloc_content_id();
        song.clip_contents
            .insert(back_content_id, back_content);
        // 両半は元 clip の共有名を引き継ぐ (split は両側を fresh content_id に
        // fork するので、 名前も両方へ複製する)。
        if !src_name.is_empty() {
            song.set_content_name(front_content_id, src_name.clone());
            song.set_content_name(back_content_id, src_name.clone());
        }

        // Mutate the clip in place: front half stays as `clip`
        // (length / content_id rewritten), and a new clip for the
        // back half is appended on the same track.
        let track = &mut song.tracks[target.track as usize];
        // 前半は in-place で元 clip の声を保持。 後半 (新 clip) は
        // その声を引き継ぐ。
        // 前半は in-place で mute を保持。 後半 (新 clip) も元 clip の mute を引き継ぐ。
        let (src_speaker, src_singer, src_style, src_talk, src_muted) = {
            let clip_mut = &mut track.clips[target.clip as usize];
            clip_mut.length_beats = front_len;
            clip_mut.content_id = front_content_id;
            (
                clip_mut.speaker_id,
                clip_mut.singer_name.clone(),
                clip_mut.style_name.clone(),
                clip_mut.talk,
                clip_mut.muted,
            )
        };
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            start_beat: clip_start + front_len,
            length_beats: back_len,
            content_id: back_content_id,
            color: src_color,
            auto_lipsync: false,
            muted: src_muted,
            speaker_id: src_speaker,
            singer_name: src_singer,
            style_name: src_style,
            talk: src_talk,
        });
        new_selection.push(target);
        new_selection.push(ClipRef {
            track: target.track,
            clip: new_idx,
        });
        true
        })
    }

}
