//! handler::voicevox — VOICEVOX 歌唱/トーク status + vocal metadata + lipsync
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::Clip;
use common::plugin_format::PluginFormat;
use common::protocol::{PluginCommand, VocalSynthFailure};

impl AppData {
    // ===== VOICEVOX 生成状態の可視化 ==========================

    /// `VoicevoxSynthStatus` IPC handler。per-plugin の busy + 失敗種別を更新する。
    /// `Unreachable` の立上りで `failing_since` を記録 (継続中は維持) して 5s 後に
    /// 「engine 未接続」へ、`Rejected` は `rejected` に理由を積んで即時「合成できない歌詞」
    /// 表示へ。成功 (`None`) で両方クリア。idle かつ失敗なしの entry は掃除して
    /// `voicevox_any_generating` 等の判定を軽く保つ。
    pub(crate) fn apply_voicevox_synth_status(
        &mut self,
        device_id: u64,
        busy: bool,
        failure: VocalSynthFailure,
    ) {
        let now = std::time::Instant::now();
        let entry = self
            .voicevox.voicevox_synth_status
            .entry(device_id)
            .or_insert(VocalSynthStatus {
                busy: false,
                failing_since: None,
                rejected: None,
            });
        entry.busy = busy;
        match failure {
            VocalSynthFailure::None => {
                entry.failing_since = None;
                entry.rejected = None;
            }
            VocalSynthFailure::Unreachable => {
                entry.failing_since.get_or_insert(now);
                entry.rejected = None;
            }
            VocalSynthFailure::Rejected { detail } => {
                entry.failing_since = None;
                entry.rejected = Some(detail);
            }
        }
        if !entry.busy && entry.failing_since.is_none() && entry.rejected.is_none() {
            self.voicevox.voicevox_synth_status.remove(&device_id);
        }
    }

    /// engine には到達できているが歌詞等を拒否した「内容エラー」の理由 (あれば最初の 1 件)。
    /// overlay が「合成できない歌詞があります」表示に使う。
    pub fn voicevox_rejected_detail(&self) -> Option<&str> {
        self.voicevox
            .voicevox_synth_status
            .values()
            .find_map(|s| s.rejected.as_deref())
    }

    /// track が持つ builtin VOICEVOX device の安定 device id (= load 済なら)。
    /// `sync_vocal_metadata` の lookup と同じ (device 実在 → loaded_slots で load 確認)。
    pub fn voicevox_plugin_id_for_track(&self, track: &common::model::Track) -> Option<u64> {
        let device_index = track.devices.iter().position(|d| {
            d.format == PluginFormat::Builtin
                && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
        })?;
        self.ipc.loaded_slots
            .get(&(track.id, device_index as u32))
            .map(|s| s.device_id)
    }

    /// track の歌唱/読み上げ WAV 合成が進行中か (= 所属 builtin VOICEVOX が busy)。
    pub fn track_wav_synthesizing(&self, track_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else {
            return false;
        };
        self.voicevox.voicevox_synth_status.get(&pid).is_some_and(|s| s.busy)
    }

    /// 出力先 (口 track) が口パク再生成中か。
    pub fn lipsync_target_generating(&self, track_id: u32) -> bool {
        self.voicevox.lipsync_inflight.contains(&track_id)
    }

    /// いずれかの VOICEVOX 生成 (WAV 合成 / 口パク) が進行中か。
    pub fn voicevox_any_generating(&self) -> bool {
        !self.voicevox.lipsync_inflight.is_empty()
            || self.voicevox.voicevox_synth_status.values().any(|s| s.busy)
    }

    /// WAV 合成中の vocal track 数 (= 全体オーバーレイの「残り N」)。
    /// track→plugin_id を直接引く (track_wav_synthesizing の id 再 find を避け O(tracks²) 回避)。
    pub fn voicevox_synth_busy_count(&self) -> usize {
        self.song_doc.song()
            .tracks
            .iter()
            .filter(|t| {
                self.voicevox_plugin_id_for_track(t)
                    .is_some_and(|pid| self.voicevox.voicevox_synth_status.get(&pid).is_some_and(|s| s.busy))
            })
            .count()
    }

    /// engine 未接続警告を出すべきか (= busy のまま failing が閾値以上継続)。
    /// engine boot (数秒) の間は failing でも警告せず「合成中」に見せ、閾値超過で切り替える。
    pub fn voicevox_engine_unreachable(&self, now: std::time::Instant) -> bool {
        self.voicevox.voicevox_synth_status.values().any(|s| {
            s.busy
                && s.failing_since
                    .is_some_and(|t| now.duration_since(t) >= VOICEVOX_ENGINE_WARNING)
        })
    }

    /// スピナーを回し続ける (= 連続再描画を要求する) べきか。engine 未接続が
    /// 確定したら static 警告にして再描画を止める (CPU spin させない)。
    pub fn voicevox_animating(&self, now: std::time::Instant) -> bool {
        self.voicevox_any_generating() && !self.voicevox_engine_unreachable(now)
    }

    /// PR-V3: track.source = Vocal で instrument に builtin VOICEVOX が
    /// load されている全 track の clip notes を `NoteMetadata` 配列に
    /// 変換し、 plugin host に `SetBuiltinPluginNoteMetadata` で送る。
    /// plugin_id 未確定 (= load 完了通知前) の track はスキップ、
    /// `SlotPluginLoadedFromChild` 受信時に再呼び出しされる。
    ///
    /// PR-V4 follow-up: vocal track が 1 つでも存在するなら VOICEVOX
    /// engine を lazy spawn する。 旧 `begin_vocal_synth` 内にあった
    /// 起動 logic を移植 (= localhost:50021 が起動済でなければ自動で
    /// spawn、 builtin plugin の HTTP synth を成功させる前提)。
    pub fn sync_vocal_metadata(&mut self) {
        let bpm = self.song_doc.song().bpm;
        let has_vocal_track = self.song_doc.song().tracks.iter().any(|t| t.is_voicevox_vocal());
        if has_vocal_track {
            self.ensure_voicevox_engine();
        }
        for track in &self.song_doc.song().tracks {
            if !track.is_voicevox_vocal() {
                continue;
            }
            // 単一デバイスチェーン: builtin VOICEVOX を chain 内に持つ device の
            // index を探す (役割別 instrument slot は撤廃、 device index で引く)。
            let Some(device_index) = track.devices.iter().position(|d| {
                d.format == PluginFormat::Builtin
                    && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
            }) else {
                continue;
            };
            // 安定 device id を loaded_slots から引く (= load 完了確認込み)。
            let Some(slot_info) = self
                .ipc.loaded_slots
                .get(&(track.id, device_index as u32))
            else {
                continue;
            };
            let host_plugin_id = slot_info.device_id;

            // 全 clip の notes を NoteMetadata 配列に flatten。 note_id は
            // (clip-internal index) を「track 内通し番号」 にしないと衝突
            // する可能性があるので、 ここでは「全 clip 連結 index」 を使う
            // (= clip 1 の note 数 + clip 2 の note index)。 PR-V2.4 で
            // 改めて clip 単位にする予定。
            let mut entries: Vec<common::plugin_metadata::NoteMetadata> = Vec::new();
            for clip in &track.clips {
                let notes: &[common::model::Note] = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.notes())
                    .unwrap_or(&[]);
                for n in notes {
                    let note_id = entries.len() as u32;
                    entries.push(common::plugin_metadata::NoteMetadata {
                        note_id,
                        // clip-relative beats を song-absolute に変換 (=
                        // VOICEVOX synth wrapper が earliest を引いて
                        // 0 起点にする、 clip 境界跨ぎでも一貫)。
                        start_beat: clip.start_beat + n.start_beat,
                        duration_beats: n.duration_beats,
                        pitch: n.pitch,
                        velocity: n.velocity,
                        lyric: n.lyric.clone().unwrap_or_default(),
                        // builtin が clip 単位で声を分けるための
                        // grouping key + per-clip 歌唱 speaker (0 = builtin 側で
                        // DEFAULT_SINGER_ID にフォールバック)。
                        clip_id: clip.id,
                        speaker_id: clip.speaker_id,
                    });
                }
            }
            // (talk) 同トラックの `ClipContent::Text` 由来の読み上げ群を集める
            // (`docs/plan_voicevox_talk.md` §3.2)。event_id は `talk_event_id(clip.id,
            // event_index)` で決定論的に導出 (sequencer の talk-trigger と同式)。空
            // テキストは両側で skip して event_id の対応を保つ。声は per-clip
            // (`clip.speaker_id` を talk style として解釈)、スケールは `clip.talk`。
            let mut talk: Vec<common::plugin_metadata::TalkMetadata> = Vec::new();
            for clip in &track.clips {
                let Some(events) = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.text_events())
                else {
                    continue;
                };
                let scales = clip.talk.unwrap_or_default();
                for (event_index, ev) in events.iter().enumerate() {
                    if ev.text.is_empty() {
                        continue;
                    }
                    talk.push(common::plugin_metadata::TalkMetadata {
                        event_id: common::plugin_metadata::talk_event_id(
                            clip.id,
                            event_index as u32,
                        ),
                        start_beat: clip.start_beat + ev.event_start_in_clip_beats,
                        text: ev.text.clone(),
                        speaker_id: clip.speaker_id,
                        speed_scale: scales.speed_scale,
                        pitch_scale: scales.pitch_scale,
                        intonation_scale: scales.intonation_scale,
                        volume_scale: scales.volume_scale,
                    });
                }
            }
            self.send_plugin(PluginCommand::SetBuiltinPluginNoteMetadata {
                device_id: host_plugin_id,
                bpm,
                entries,
                talk,
            });
        }
    }

    /// `target` (口 track) の口パク出力を決定する入力すべての 64-bit fingerprint。
    /// `regenerate_lipsync_for_track` が phoneme query / `build_mouth_events` に渡す
    /// 入力と **厳密に一致** させる (= ここに含めた値が変わったときだけ再生成が要る):
    ///
    /// - song の `bpm`
    /// - `target` の `mouth_map` (7 slot の `ImageSourceId`)
    /// - `target` を出力先にする全ソーストラックの **並び順 (priority)** と、各 clip の
    ///   `start_beat` / `length_beats`、および
    ///     - sing clip: notes の `start_beat` / `duration_beats` / `pitch` / `lyric`
    ///       (= `build_sing_query` が読むフィールド。`velocity` / `muted` は phoneme へ
    ///       影響しないので **含めない**)
    ///     - talk clip: 先頭の非空 `TextEvent` の `text` / `event_start_in_clip_beats`、
    ///       および clip の `speaker_id` / `talk.speed_scale` (= `query_talk_phonemes` が
    ///       phoneme 長に使う値。pitch / intonation / volume は無関係なので含めない)
    ///
    /// track 名 / 色 / mute / volume / plugin 等の **非入力** は含めないので、それらの
    /// 編集では fingerprint が不変 → `LipsyncDebounceFired` が再生成をスキップする。
    /// 走査順は下の `regenerate_lipsync_for_track` の snap 収集ループと一致させること。
    pub(crate) fn lipsync_input_fingerprint(song: &common::model::Song, target_id: u32) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        song.bpm.to_bits().hash(&mut h);
        if let Some(t) = song.tracks.iter().find(|t| t.id == target_id)
            && let Some(m) = &t.mouth_map
        {
            [m.a, m.i, m.u, m.e, m.o, m.n, m.closed].hash(&mut h);
        }
        for (idx, src) in song.tracks.iter().enumerate() {
            if src.lipsync_target_track != Some(target_id) {
                continue;
            }
            (idx as u32).hash(&mut h); // priority (= トラック並び順)
            // snap を生成する clip (notes 有り / 非空 text 有り) だけが出力に効く。
            // `regenerate_lipsync_for_track` の収集と同条件で、その clip の位置と
            // 内容のみをハッシュする (= 非対象 clip の移動では fingerprint 不変)。
            for clip in &src.clips {
                let content = song.clip_contents.get(&clip.content_id);
                if let Some(notes) = content.and_then(|c| c.notes()).filter(|n| !n.is_empty()) {
                    // sing: clip 位置 + build_sing_query が読む note フィールドのみ。
                    clip.start_beat.to_bits().hash(&mut h);
                    clip.length_beats.to_bits().hash(&mut h);
                    for n in notes {
                        n.start_beat.to_bits().hash(&mut h);
                        n.duration_beats.to_bits().hash(&mut h);
                        n.pitch.hash(&mut h);
                        n.lyric.hash(&mut h);
                    }
                } else if let Some(ev) = content
                    .and_then(|c| c.text_events())
                    .and_then(|events| events.iter().find(|e| !e.text.is_empty()))
                {
                    // talk: clip 位置 + 先頭の非空 TextEvent + 声 + 話速のみ。
                    clip.start_beat.to_bits().hash(&mut h);
                    clip.length_beats.to_bits().hash(&mut h);
                    ev.text.hash(&mut h);
                    ev.event_start_in_clip_beats.to_bits().hash(&mut h);
                    clip.speaker_id.hash(&mut h);
                    clip.talk
                        .unwrap_or_default()
                        .speed_scale
                        .to_bits()
                        .hash(&mut h);
                }
            }
        }
        h.finish()
    }

    /// 全 target (口) track の現在の入力 fingerprint を記録する。load 直後に呼び、
    /// 保存済み口パク clip を生成した入力をベースライン化することで、開いた直後の
    /// 非入力編集 (track rename 等) で口パクが再生成されないようにする。
    pub(crate) fn seed_lipsync_fingerprints(&mut self) {
        self.voicevox.lipsync_fingerprints.clear();
        let mut targets: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter_map(|t| t.lipsync_target_track)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        for target in targets {
            let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target);
            self.voicevox.lipsync_fingerprints.insert(target, fp);
        }
    }

    /// 口パク (lip-sync) を再生成する (docs/plan_pakupaku.md §7)。`vocal_track_id`
    /// の各 clip の notes を snapshot し、背景スレッドで `query_phonemes`
    /// (`sing_frame_audio_query` のみ) を叩いて結果を `AppEvent::LipsyncGenerated`
    /// で main thread へ返す。binding (`lipsync_target_track`) 未設定 / 口 track の
    /// `mouth_map` 未設定 / notes を持つ clip 無し のときは no-op。歌唱のみ (Q6)。
    pub fn regenerate_lipsync_for_track(&mut self, vocal_track_id: u32) {
        let Some(target_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == vocal_track_id)
            .and_then(|t| t.lipsync_target_track)
        else {
            return;
        };
        // この target の現在の入力 fingerprint をベースラインとして記録する。
        // `LipsyncDebounceFired` はこの値と現在値を比べ、変化した target だけ
        // 再生成する (= rename 等の非入力編集では再生成しない)。直接呼び出し
        // (binding / mouth_map 変更) はここで必ず最新値へ更新されるので、直後の
        // debounce 発火は fingerprint 一致で二重再生成にならない。
        let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target_id);
        self.voicevox.lipsync_fingerprints.insert(target_id, fp);
        // 口 track が存在し mouth_map が設定済みか (= 生成する意味があるか)。
        let configured = self.song_doc.song().tracks.iter().any(|t| {
            t.id == target_id && t.mouth_map.as_ref().is_some_and(|m| m.is_configured())
        });
        if !configured {
            return;
        }
        let bpm = self.song_doc.song().bpm;
        let lead_in = common::lipsync::lead_in_beats(bpm);
        // (talk) target 中心: 出力先が `target_id` の **全ソーストラック** をまとめて
        // 再生成する (`docs/plan_voicevox_talk.md`)。トラック並び順 index を priority に
        // し、apply 側で重なりを上位優先で解決する。各 clip は notes (歌唱) があれば sing、
        // 無く Text なら talk として扱う。
        let mut snaps: Vec<(f64, f64, f64, u32, Vec<common::model::Note>)> = Vec::new();
        let mut talk_snaps: Vec<(f64, f64, f64, u32, String, u32, common::model::TalkParams)> =
            Vec::new();
        for (idx, src) in self.song_doc.song().tracks.iter().enumerate() {
            if src.lipsync_target_track != Some(target_id) {
                continue;
            }
            let priority = idx as u32;
            for clip in &src.clips {
                // sing: notes を持つ clip。
                if let Some(notes) = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.notes())
                    && !notes.is_empty()
                {
                    let first_note_local_beat = notes
                        .iter()
                        .map(|n| n.start_beat)
                        .fold(f64::INFINITY, f64::min);
                    snaps.push((
                        clip.start_beat,
                        clip.length_beats,
                        first_note_local_beat,
                        priority,
                        notes.to_vec(),
                    ));
                    continue;
                }
                // talk: Text clip の先頭の非空 TextEvent。
                if let Some(events) = self
                    .song_doc.song()
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.text_events())
                    && let Some(ev) = events.iter().find(|e| !e.text.is_empty())
                {
                    talk_snaps.push((
                        clip.start_beat,
                        clip.length_beats,
                        ev.event_start_in_clip_beats + lead_in,
                        priority,
                        ev.text.clone(),
                        clip.speaker_id,
                        clip.talk.unwrap_or_default(),
                    ));
                }
            }
        }
        if snaps.is_empty() && talk_snaps.is_empty() {
            return;
        }
        self.ensure_voicevox_engine();
        // 出力先 (口 track) を in-flight に登録 = クリップ上スピナー +
        // 全体オーバーレイ「口パク生成中」を点灯。完了イベントで必ず外す。
        self.voicevox.lipsync_inflight.insert(target_id);
        // spawn 時点の世代を snapshot し、 結果と一緒に返す。 HTTP が遅延して
        // いる間に project が切り替わる (reset_saved_baseline が gen を bump) と
        // handler 側で破棄される。
        let generation = self.voicevox.lipsync_gen;
        let target_track_id = target_id;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            let mut clips = Vec::with_capacity(snaps.len() + talk_snaps.len());
            for (clip_start_beat, clip_len_beats, first_note_local_beat, priority, notes) in snaps {
                match crate::voicevox_client::query_phonemes(&notes, bpm) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_note_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "lip-sync phoneme query failed");
                    }
                }
            }
            // (talk) 読み上げ phoneme を `query_talk_phonemes` で取り、同じ
            // `LipsyncClipResult` に詰める (= apply 経路は歌唱と共通)。
            for (clip_start_beat, clip_len_beats, first_note_local_beat, priority, text, speaker_id, scales) in
                talk_snaps
            {
                match crate::voicevox_client::query_talk_phonemes(&text, speaker_id, &scales) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_note_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "talk lip-sync phoneme query failed");
                    }
                }
            }
            // 成功/失敗/空に関わらず **必ず** 送る。handler が
            // `lipsync_inflight` から target を外してスピナーを止める。clips が空なら
            // (= 全 HTTP 失敗) handler 側で「既存 clip を温存」して反映だけスキップする。
            proxy.send(AppEvent::LipsyncGenerated {
                vocal_track_id,
                target_track_id,
                bpm,
                clips,
                generation,
            });
        });
    }

    /// `AppEvent::LipsyncGenerated` handler。口 track の自動生成 clip
    /// (`auto_lipsync == true`) を全て差し替える。派生データなので Undo
    /// snapshot は積まない (user 編集側が Undo 対象、手編集は保持しない = Q8)。
    pub(crate) fn apply_lipsync_generated(
        &mut self,
        vocal_track_id: u32,
        bpm: f32,
        results: Vec<LipsyncClipResult>,
    ) {
        // 口パクは派生データの自動再生成なので undo 履歴に入れない
        // (normalize_song)。 epoch は進む = dirty + frame flush の再 sync は走る。
        // normalize_song 経由で export_lock を同期する: export (freewheel /
        // video render) 中に口パク再生成が完了しても render 中の LoadSong を
        // 送らない (H2)。
        self.normalize_song(|song| {
            // HTTP 中に song が変わっている可能性があるため id ベースで再解決。
            let Some(target_id) = song
                .tracks
                .iter()
                .find(|t| t.id == vocal_track_id)
                .and_then(|t| t.lipsync_target_track)
            else {
                return;
            };
            let Some(m_idx) = song.tracks.iter().position(|t| t.id == target_id) else {
                return;
            };
            let Some(mouth_map) = song.tracks[m_idx].mouth_map.clone() else {
                return;
            };
            // 既存の自動生成 clip を全削除 (手編集保持しない)。
            song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
            let res = song.video_resolution;
            // (talk) 全ソースの mouth event を song-absolute (start, end, image_id, priority) に
            // 展開する。複数ソース (歌唱 Vox / 読み上げ Talk) が同じ口 track を共有しても、
            // 次の merge で重なりが上位 (priority 小 = 上のトラック) 優先で解決され、口画像が
            // 二重表示にならない (`docs/plan_voicevox_talk.md`)。
            let mut spans: Vec<(f64, f64, u32, u32)> = Vec::new();
            for r in &results {
                let events = common::lipsync::build_mouth_events(
                    &r.phonemes,
                    &mouth_map,
                    bpm,
                    r.first_note_local_beat,
                    r.clip_len_beats,
                );
                for ev in events {
                    let s = r.clip_start_beat + ev.event_start_in_clip_beats;
                    let e = s + ev.event_length_beats;
                    if e > s {
                        spans.push((s, e, ev.source_id, r.priority));
                    }
                }
            }
            // 上位優先で重なりを解決した非重複 mouth event 列。これを 1 本の auto_lipsync
            // Image clip にまとめて口 track へ置く (event 間の隙間 = 口画像なし = 自然)。
            let merged = merge_lipsync_events_by_priority(spans);
            if !merged.is_empty() {
                let clip_start = merged.iter().map(|m| m.0).fold(f64::INFINITY, f64::min);
                let clip_end = merged.iter().map(|m| m.1).fold(f64::NEG_INFINITY, f64::max);
                let mut events: Vec<common::model::ImageEvent> = Vec::with_capacity(merged.len());
                for (s, e, img) in merged {
                    let mut ev = common::model::ImageEvent {
                        source_id: img,
                        event_start_in_clip_beats: s - clip_start,
                        event_length_beats: e - s,
                        ..common::model::ImageEvent::default()
                    };
                    // build_mouth_events は rect を全画面 default で返すので、素材寸法から
                    // aspect-fit rect を計算して上書き (立ち絵の他の子レイヤーと収まりを揃える)。
                    if let Some(src) = song.media.image_sources.get(&img) {
                        let (x, y, w, h) = aspect_fit_pip_rect(res, (src.width, src.height));
                        ev.x = x;
                        ev.y = y;
                        ev.w = w;
                        ev.h = h;
                    }
                    events.push(ev);
                }
                let content_id = song.alloc_content(
                    common::model::ClipContent::Image(common::model::ImageContent { events }),
                    "口パク".to_string(),
                );
                let m = &mut song.tracks[m_idx];
                let clip_id = m.alloc_clip_id();
                m.clips.push(Clip {
                    id: clip_id,
                    start_beat: clip_start,
                    length_beats: clip_end - clip_start,
                    content_id,
                    color: None,
                    auto_lipsync: true,
                    ..Default::default()
                });
            }
            // 削除した古い clip の content を回収。
            song.gc_clip_contents();
        });
    }

    /// `SetLipsyncTarget` handler。vocal track の出力先 binding を更新し、
    /// 設定時は口パクを再生成する (snapshot は `is_undoable` 経由で handler
    /// 前に取得済み = binding 変更を undo 可能)。
    pub(crate) fn set_lipsync_target(&mut self, track_id: u32, target: Option<u32>) {
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            t.lipsync_target_track = target;
            true
        }) == Some(true);
        if applied && target.is_some() {
            self.regenerate_lipsync_for_track(track_id);
        }
    }

    /// `SetMouthMapSlot` handler。口 track の `mouth_map` の 1 slot を更新し、
    /// この口 track を出力先にしている vocal track の口パクを再生成する。
    pub(crate) fn set_mouth_map_slot(
        &mut self,
        track_id: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    ) {
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let map = t
                .mouth_map
                .get_or_insert_with(common::model::MouthMap::default);
            match shape {
                common::model::MouthShape::A => map.a = source_id,
                common::model::MouthShape::I => map.i = source_id,
                common::model::MouthShape::U => map.u = source_id,
                common::model::MouthShape::E => map.e = source_id,
                common::model::MouthShape::O => map.o = source_id,
                common::model::MouthShape::N => map.n = source_id,
                common::model::MouthShape::Closed => map.closed = source_id,
            }
            true
        }) == Some(true);
        if !applied {
            return;
        }
        // この口 track を出力先にしている vocal track を再生成する。
        let vocal_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|v| v.lipsync_target_track == Some(track_id))
            .map(|v| v.id)
            .collect();
        for vid in vocal_ids {
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// song 変更時に呼ぶ (= `flush_song_sync` から)。binding を持つ
    /// vocal track があれば debounce timer を立て、quiet period (400ms) 後に
    /// `LipsyncDebounceFired` を送る。rapid 編集 (歌詞タイプ等) は世代カウンタで
    /// coalesce され、最後の 1 回だけ再生成される。
    /// 進行中 (debounce 待ち) の口パク自動再生成を無効化する。 generation
    /// counter を bump するだけで、 既にスケジュール済みの `LipsyncDebounceFired`
    /// は世代不一致になり handler 側で no-op になる (新しい timer は spawn しない)。
    /// `reset_saved_baseline` (= load / new / recovery) から呼び、 開いた直後の
    /// spurious dirty を防ぐ。
    pub(crate) fn cancel_pending_lipsync_regen(&mut self) {
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
    }

    pub(crate) fn mark_lipsync_dirty(&mut self) {
        if !self
            .song_doc.song()
            .tracks
            .iter()
            .any(|t| t.lipsync_target_track.is_some())
        {
            return;
        }
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
        let generation = self.voicevox.lipsync_gen;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            proxy.send(AppEvent::LipsyncDebounceFired(generation));
        });
    }

}
