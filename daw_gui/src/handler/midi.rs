//! handler::midi — MIDI 入力 / learn / binding / step & realtime 録音
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::protocol::{AudioCommand};

impl AppData {
    // -------- Clip / note / midi -------------------------------------------

    /// Phase 7 B4 Step D (2026-05-13): MIDI input note_on dispatcher。
    /// `midi_recording` で arm 録音 path、 そうでなければ既存 step-input mode。
    pub(crate) fn handle_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        if self.recording.midi_recording {
            self.record_midi_note_on(pitch, velocity);
        } else {
            self.step_input_note_on(pitch, velocity);
        }
    }

    /// Phase 7 B4 Step D: MIDI input note_off dispatcher。 録音中は length 確定、
    /// step-input mode は no-op (既存挙動)。
    pub(crate) fn handle_midi_note_off(&mut self, pitch: u8) {
        if self.recording.midi_recording {
            self.record_midi_note_off(pitch);
        }
    }

    /// MIDI Learn button (transport) が bind する target を決める (B2 / r.md #8、
    /// touch + learn)。 直近に触った param (`last_touched_param`) が bind 可能
    /// (PluginParam / track Volume / Pan) ならそれを優先、 無ければ選択 track の
    /// Volume に fallback。
    pub fn midi_learn_binding_target(
        &self,
        armed_track: Option<u32>,
    ) -> Option<common::model::BindingTarget> {
        use common::model::{AutomationTarget, BindingTarget, TrackBuiltinParam};
        if let Some(tp) = &self.ui_ephemeral.last_touched_param {
            match tp.target {
                AutomationTarget::PluginParam { device_id, param_id, .. } => {
                    return Some(BindingTarget::PluginParam {
                        track: tp.track_id,
                        device_id,
                        param_id,
                        legacy_device_index: None,
                    });
                }
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => {
                    return Some(BindingTarget::TrackVolume(tp.track_id));
                }
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => {
                    return Some(BindingTarget::TrackPan(tp.track_id));
                }
                _ => {}
            }
        }
        armed_track.map(BindingTarget::TrackVolume)
    }

    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn 経路 + 通常 lookup 経路。
    /// `midi_learn_target` Some なら新規 binding 追加 (= 同じ
    /// `(channel, controller)` 既存 entry は replace、 1 度の CC 受信で None
    /// に戻る)、 None なら `Song.midi_bindings` を lookup して match した
    /// 全 target に値を送る。 channel = 16 は any-channel match。
    pub(crate) fn handle_midi_control_change(
        &mut self,
        channel: u8,
        controller: u8,
        value: u8,
    ) {
        if let Some(target) = self.recording.midi_learn_target.take() {
            // Learn mode: 既存 同 (channel, controller) を retain で除外 +
            // 新 binding push。 status_message は次 frame の通常 status に上書き
            // されるが「bind 完了」 を一瞬表示。
            self.edit_song(move |song| {
                song.midi_bindings.retain(|b| {
                    !(b.controller == controller && b.channel == channel)
                });
                song.midi_bindings.push(common::model::MidiBinding {
                    channel,
                    controller,
                    target,
                });
            });
            self.ui_ephemeral.status_message =
                format!("MIDI bind: CC {controller} (ch {channel}) → {target:?}");
            return;
        }
        // 通常 lookup: 該当 binding 全てに値送信 (= 同 CC を複数 target に
        // bind する usage を許容)。 channel = 16 は any-channel match。
        let targets: Vec<common::model::BindingTarget> = self
            .song_doc.song()
            .midi_bindings
            .iter()
            .filter(|b| {
                b.controller == controller
                    && (b.channel == channel || b.channel == 16)
            })
            .map(|b| b.target)
            .collect();
        for target in targets {
            self.apply_midi_value_to_target(target, value);
        }
    }

    /// Phase 7 B1-M Step 2: CC 値 (0..127) を target に適用。 normalization は
    /// target ごとに違う (= TrackVolume は 0..1、 TrackPan は -1..1、
    /// SongTempo は 60..180 BPM linear)。 既存 setter (set_track_volume /
    /// set_track_pan / song.bpm + IPC) を経由するので audio engine 反映も
    /// automatic。
    pub(crate) fn apply_midi_value_to_target(
        &mut self,
        target: common::model::BindingTarget,
        value: u8,
    ) {
        let v_norm = f32::from(value.min(127)) / 127.0;
        match target {
            common::model::BindingTarget::TrackVolume(track_id) => {
                self.set_track_volume(track_id, v_norm.clamp(0.0, 1.0));
            }
            common::model::BindingTarget::TrackPan(track_id) => {
                let pan = (v_norm * 2.0 - 1.0).clamp(-1.0, 1.0);
                self.set_track_pan(track_id, pan);
            }
            common::model::BindingTarget::SongTempo => {
                // CC 0..127 → 60..180 BPM linear。 Raw audio clip を秒固定スケール
                // (r.md #7)。Raw clip があれば LoadSong (decode 再利用で軽量) で
                // 再生 window を追従、無ければ軽量 SetSongBpm で即時更新。
                let bpm = (60.0 + v_norm * 120.0).clamp(1.0, 400.0);
                let old_bpm = self.song_doc.song().bpm;
                self.edit_song(|song| song.bpm = bpm);
                if !self.rescale_raw_clips_for_bpm_change(old_bpm, bpm) {
                    self.send_audio(AudioCommand::SetSongBpm { bpm });
                }
                // plugin host 側の BPM 消費者 (VOICEVOX metadata / ARA / lipsync)
                // は edit_song の epoch bump を runner の frame flush が拾って追従する
                // (旧 pending_host_sync coalesce を epoch 一本化で置換)。
            }
            common::model::BindingTarget::PluginParam {
                track,
                device_id,
                param_id,
                ..
            } => {
                // B2 (r.md #8): CC → plugin param。 param range で CC 0..1 を実
                // value_real (min..max) に変換し、 inspector knob と同じ lane-
                // default 経路 (`set_plugin_param_on_track`) で更新 → host へ再
                // sync して音に反映。 range 未取得 (host が PluginParamList 未送)
                // は 0..1 を plain とみなす best-effort。
                // v29: binding は安定 device_id を持つ。 現在位置 (track /
                // index) へは逆引きで繋ぐ (device 削除済みなら無視)。
                let Some((resolved_track, device_index)) =
                    find_device_by_id(self.song_doc.song(), device_id)
                else {
                    return;
                };
                let _ = track;
                let target = common::model::AutomationTarget::PluginParam {
                    device_id,
                    param_id,
                    legacy_device_index: None,
                };
                let value_real = match self.plugin_param_range(resolved_track, &target) {
                    Some((min, max)) => min + f64::from(v_norm) * (max - min),
                    None => f64::from(v_norm),
                };
                self.set_plugin_param_on_track(resolved_track, device_index, param_id, value_real);
                // set_plugin_param_on_track が edit_song で epoch を bump するので、
                // 毎 CC の full LoadSong flood は runner の frame flush (flush_song_sync)
                // が 1 frame 1 回へ構造的に coalesce する (旧 pending_host_sync 置換)。
            }
        }
    }

    /// 既存 step-input mode (= selected_clip + step_cursor_beat に固定 length
    /// で 1 note ずつ手動入力)。 midi_recording == false のときだけ走る。
    pub(crate) fn step_input_note_on(&mut self, pitch: u8, velocity: u8) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let cursor = self.recording.step_cursor_beat;
        let step = self.recording.step_size_beats;
        let target_track_idx = target.track as usize;
        let target_clip_idx = target.clip as usize;
        let Some(clip) = self
            .song_doc.song()
            .tracks
            .get(target_track_idx)
            .and_then(|t| t.clips.get(target_clip_idx))
        else {
            return;
        };
        // r.md #44: step カーソルは content-local。 clip が見せている窓
        // `[offset, offset + length)` の中を進み、外れたら窓の先頭へ戻す
        // (`selected_clip` 切替時の 0 初期化も窓外なのでここで吸収される)。
        let (win_start, win_end) = clip.content_window();
        let cursor = if cursor < win_start || cursor >= win_end {
            win_start
        } else {
            cursor
        };
        let Some(Some(selected)) = self.edit_song(|song| {
            let content =
                midi_content_in_clip_mut(song, target_track_idx, target_clip_idx)?;
            // v29: 新規 note は allocator で安定 id を採番する。
            let note_id = content.alloc_note_id();
            let notes = &mut content.notes;
            let new_idx = notes.len() as u32;
            notes.push(common::model::Note {
                id: note_id,
                start_beat: cursor,
                duration_beats: step,
                pitch,
                velocity,
                lyric: None,
                muted: false,
            });
            // ステップ入力した note を勝者として重なり解消。
            let remap = resolve_note_overlaps(notes, &[new_idx]);
            Some(remap_indices(&remap, &[new_idx]))
        }) else {
            return;
        };
        let next_cursor = cursor + step;
        // selected_notes は packed note id。入力先 (target) clip の slot で pack。
        self.selection.selected_notes = self.pack_clip_selection(target, &selected);
        self.recording.step_cursor_beat = next_cursor;
    }

    /// Phase 7 B4 Step D: 録音中の note_on 処理。 armed track 全てに対して
    /// playhead 位置の MIDI clip (無ければ新規作成 / 末尾近ければ延長) に
    /// 仮 length 0.05 beat の note を push + active_notes に start_beat
    /// 記録 (= note_off で length 上書き)。 既存 clip の content_id 共有
    /// (linked clip) の場合、 sibling にも自動反映 (= ContentId 共有の意図、
    /// 録音書き込みでも同じ動作、 不都合なら別 phase で「録音前に
    /// make_unique」 を検討)。
    pub(crate) fn record_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        let playhead =
            self.transport.playhead_beat.map(f64::from).unwrap_or(0.0);
        if playhead < 0.0 {
            return;
        }
        // Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input。
        // note_off も同じ snap を適用するので、 deterministic snap で
        // (track_id, pitch) lookup が整合する。 step input は別 path
        // (`step_input_note_on`) を経由するのでここの snap は録音時のみ。
        let pitch = if self.recording.snap_live_input {
            self.song_doc.song()
                .scale_at(playhead)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
        let armed_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|t| t.armed)
            .map(|t| t.id)
            .collect();
        for track_id in armed_ids {
            self.recording.midi_recording_active_notes
                .insert((track_id, pitch), playhead);
            self.ensure_midi_clip_at_playhead(track_id, playhead);
            let Some((track_idx, clip_idx)) =
                self.find_midi_clip_at_playhead(track_id, playhead)
            else {
                continue;
            };
            // r.md #44: note は content-local なので、原点 (= clip 開始 - 窓 offset)
            // を引いて local 化する。
            let clip_origin = self.song_doc.song().tracks[track_idx].clips[clip_idx]
                .content_origin_beat();
            let local_start = playhead - clip_origin;
            self.edit_song(|song| {
                if let Some(content) = midi_content_in_clip_mut(song, track_idx, clip_idx) {
                    // v29: 録音で入る note も allocator で安定 id を採番。
                    let note_id = content.alloc_note_id();
                    content.notes.push(common::model::Note {
                        id: note_id,
                        start_beat: local_start,
                        duration_beats: 0.05,
                        pitch,
                        velocity,
                        lyric: None,
                        muted: false,
                    });
                }
            });
        }
    }

    /// Phase 7 B4 Step D: 録音中の note_off 処理。 active_notes から start
    /// を取り出し、 length = playhead - start で確定。 該当 note を
    /// `start_beat` (clip-local) + `pitch` で再 search (= note_on 時に
    /// active_notes に track-domain start を保存しているので、 clip-local
    /// に戻して check)。
    pub(crate) fn record_midi_note_off(&mut self, pitch: u8) {
        let playhead =
            self.transport.playhead_beat.map(f64::from).unwrap_or(0.0);
        // Phase 7 B5: note_on 側で snap した pitch で active_notes に登録して
        // いるので、 note_off の lookup key も同じ snap を適用。 snap は
        // deterministic なので転調を跨がない note なら lookup は必ず hit する。
        let pitch = if self.recording.snap_live_input {
            self.song_doc.song()
                .scale_at(playhead)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
        let armed_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|t| t.armed)
            .map(|t| t.id)
            .collect();
        for track_id in armed_ids {
            let Some(start) = self
                .recording.midi_recording_active_notes
                .remove(&(track_id, pitch))
            else {
                continue;
            };
            let Some((track_idx, clip_idx)) =
                self.find_midi_clip_containing_beat(track_id, start)
            else {
                continue;
            };
            let clip_origin = self.song_doc.song().tracks[track_idx].clips[clip_idx]
                .content_origin_beat();
            let local_start = start - clip_origin;
            let length = (playhead - start).max(0.05);
            self.edit_song(|song| {
                if let Some(notes) = song.notes_in_clip_mut(track_idx, clip_idx)
                    && let Some(n) = notes.iter_mut().find(|n| {
                        (n.start_beat - local_start).abs() < 1e-6
                            && n.pitch == pitch
                    })
                {
                    n.duration_beats = length;
                }
            });
        }
    }

    /// playhead 位置に armed track 用 MIDI clip があれば何もしない、 末尾
    /// 直近 (= 1 beat 以内) なら延長、 それ以外なら新規 clip を playhead
    /// 位置に作成 (length 4 beat、 ContentId 新規採番 + clip_contents 登録)。
    pub(crate) fn ensure_midi_clip_at_playhead(
        &mut self,
        track_id: u32,
        playhead: f64,
    ) {
        self.edit_song_checked(|song| {
            let Some(track_idx) = song.tracks.iter().position(|t| t.id == track_id) else {
                return false;
            };
            // 既存 clip 内ならそのまま。
            if song.tracks[track_idx].clips.iter().any(|c| {
                playhead >= c.start_beat && playhead < c.start_beat + c.length_beats
            }) {
                return false;
            }
            // 末尾の直近 1 beat 以内なら延長。
            if let Some(clip) = song.tracks[track_idx]
                .clips
                .iter_mut()
                .find(|c| {
                    let end = c.start_beat + c.length_beats;
                    playhead >= end && playhead - end <= 1.0
                })
            {
                clip.length_beats = playhead - clip.start_beat + 4.0;
                return true;
            }
            // 新規 clip 作成。
            let cid = song.alloc_content_id();
            song.clip_contents.insert(
                cid,
                common::model::ClipContent::Midi(common::model::MidiContent {
                    notes: vec![],
                    next_note_id: 1,
                }),
            );
            let track = &mut song.tracks[track_idx];
            let new_clip_id = track.next_clip_id;
            track.next_clip_id += 1;
            let new_clip = common::model::Clip {
                id: new_clip_id,
                start_beat: playhead,
                length_beats: 4.0,
                content_id: cid,
                ..Default::default()
            };
            track.clips.push(new_clip);
            true
        });
        // content_name は **明示 rename 専用**。 ここで自動名
        // ("Recorded N") を入れると、 後でノートに歌詞が付いたとき明示名優先
        // ルールで歌詞を隠してしまう (= ⑤⑦ の再来)。 生成クリップは
        // `create_clip` と同様 **無名** で作り、 表示名は歌詞 / 本文から導出する
        // (`clip_display_label`)。 名前が要るならユーザーが rename する。
    }

    /// playhead が clip 範囲 `[start_beat, start_beat + length_beats)` に
    /// 含まれる MIDI clip の (track_idx, clip_idx) を返す。 無ければ None。
    pub(crate) fn find_midi_clip_at_playhead(
        &self,
        track_id: u32,
        playhead: f64,
    ) -> Option<(usize, usize)> {
        let track_idx =
            self.song_doc.song().tracks.iter().position(|t| t.id == track_id)?;
        let track = &self.song_doc.song().tracks[track_idx];
        let clip_idx = track.clips.iter().position(|c| {
            playhead >= c.start_beat
                && playhead < c.start_beat + c.length_beats
        })?;
        Some((track_idx, clip_idx))
    }

    /// 指定 beat 位置を含む MIDI clip の (track_idx, clip_idx)。
    /// `find_midi_clip_at_playhead` と同等だが、 引数を意味的に区別する
    /// (= note_off 時は note_on 時刻、 note_on 時は playhead を渡す)。
    pub(crate) fn find_midi_clip_containing_beat(
        &self,
        track_id: u32,
        beat: f64,
    ) -> Option<(usize, usize)> {
        self.find_midi_clip_at_playhead(track_id, beat)
    }

    /// Phase 7 B4 Step C/D: Record toggle button click。 既に recording
    /// (含 pending) なら stop、 idle なら start。
    pub(crate) fn toggle_midi_recording(&mut self) {
        if self.recording.midi_recording || self.recording.midi_recording_pending {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    /// Phase 7 B4 Step C: 録音開始。 `count_in_bars > 0` なら preroll +
    /// metronome 強制 ON、 0 なら即時 recording。 いずれも `Play` を audio に
    /// 送る。 metronome の元状態は `metronome_enabled_pre_recording` に snap。
    pub(crate) fn start_recording(&mut self) {
        if self.recording.midi_recording || self.recording.midi_recording_pending {
            return;
        }
        // (review) 録音 take を 1 undo step にする。 `MidiNoteOn` / 録音 point
        // 挿入は per-event で snapshot しない (is_undoable 外) ので、 ここで
        // 1 回だけ積む — これが無いと Ctrl+Z が「録音 + 直前の別編集」 を
        // まとめて巻き戻す。
        self.recording.midi_recording_active_notes.clear();
        // ensure-synced: StartCountIn の preroll (bpm→sample) と Play は engine の
        // 現 song を前提にする。 録音開始直前の編集が未 flush なら先に届ける
        // (epoch 未変化なら no-op)。
        self.flush_song_sync();
        let bars = self.recording.count_in_bars;
        self.recording.metronome_enabled_pre_recording = Some(self.transport.metronome_enabled);
        if bars > 0 {
            if !self.transport.metronome_enabled {
                // count-in 強制 ON。 既存 SetMetronomeEnabled handler を
                // 呼び出して audio engine への IPC 送信もまとめて。
                self.handle_event(AppEvent::SetMetronomeEnabled(true));
            }
            let beats_per_bar = f64::from(self.song_doc.song().time_sig.0.max(1));
            let preroll_beats = f64::from(bars) * beats_per_bar;
            let sr = f64::from(self.ipc.sample_rate);
            let bpm = f64::from(self.song_doc.song().bpm.max(1.0));
            let preroll_samples =
                (preroll_beats * 60.0 / bpm * sr).round().max(0.0) as u64;
            self.send_audio(AudioCommand::StartCountIn {
                samples: preroll_samples,
            });
            // Rec ボタンの残り小節数表示用。 最初の Tick が届くまでの数フレームも
            // 正しい値 (= 満タン) を出せるよう remaining も同じ値で初期化する。
            self.recording.count_in_total_samples = preroll_samples;
            self.recording.count_in_remaining_samples = preroll_samples;
            self.recording.midi_recording_pending = true;
        } else {
            self.recording.midi_recording = true;
        }
        self.send_audio(AudioCommand::Play);
    }

    /// Phase 7 B4 Step C/D: 録音停止 / cancel。 active_notes 全 clear、
    /// metronome を recording 開始前の状態に戻す、 audio engine の preroll を
    /// `StartCountIn { samples: 0 }` で cancel (= count-in 中の stop に対応)。
    pub(crate) fn stop_recording(&mut self) {
        let was_active =
            self.recording.midi_recording || self.recording.midi_recording_pending;
        self.recording.midi_recording = false;
        self.recording.midi_recording_pending = false;
        self.recording.count_in_total_samples = 0;
        self.recording.count_in_remaining_samples = 0;
        self.recording.midi_recording_active_notes.clear();
        if let Some(prev) =
            self.recording.metronome_enabled_pre_recording.take()
            && prev != self.transport.metronome_enabled
        {
            self.handle_event(AppEvent::SetMetronomeEnabled(prev));
        }
        if was_active {
            // count-in 中の cancel もしくは録音終了。 audio engine 側 preroll
            // を 0 で上書きして count-in 即時終了。 normal 録音終了の場合は
            // preroll は既に 0 なので no-op。
            self.send_audio(AudioCommand::StartCountIn { samples: 0 });
        }
    }

}
