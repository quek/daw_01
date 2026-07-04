//! handler::transport — 再生 / 停止 / loop / panic / seek
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use common::protocol::{AudioCommand};

impl AppData {
    // -------- Playback -----------------------------------------------------

    pub(crate) fn play(&mut self) {
        // export 中は再生を禁止する。音声 freewheel フェーズの realtime play は
        // offline render と競合し、書き出される音声を壊しうる（映像フェーズは
        // 独立だが、 混乱を避けて export 全体で一律に止める）。標準 WAV export も
        // `export_stage` が立つので同じ gate で止まる（旧構造では WAV export 中に
        // 再生できてしまい render を壊しえた）。
        if self.transport.pending_video_export.is_some() || self.transport.export_stage.is_some() {
            self.ui_ephemeral.status_message = "書き出し中は再生できません".into();
            return;
        }
        // プロジェクトロードの asset decode 中は音声がまだ揃って
        // いないので再生を gate して queue する (load 完了で on_asset_decode_tick
        // が flush)。
        if self.media.asset_decode.is_some() {
            self.transport.pending_play = true;
            self.ui_ephemeral.status_message = "プロジェクト読込中...".into();
            return;
        }
        // A7: if any plugin is still in the SetSlotPlugin →
        // SlotPluginLoaded round-trip (its `OpenPluginShmem` may not
        // have reached the audio engine yet), queue the Play so every
        // track starts on the same buffer once registration completes.
        // Without this the just-loaded tracks render silent for the
        // first few buffers / first loop.
        if !self.ipc.pending_plugin_loads.is_empty() {
            self.transport.pending_play = true;
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
            return;
        }
        // ensure-synced (docs/plan_arch_refactor.md §7.5): 同 frame 内に編集があって
        // まだ frame flush 前なら、 最新 song を Play の前に engine へ届ける。 epoch
        // 未変化なら no-op なので、 定常状態 (= 既に frame flush 済) では LoadSong を
        // 再送せず、 大量 WAV の compile_audio_schedule 同期遅延を踏まない。
        self.flush_song_sync();
        // Pro Tools 流の「Stop で開始位置に戻る」 用に、 実際の再生
        // 開始時の playhead を保存。 ruler クリック等で playhead を
        // 移動してから play した場合は、 その位置が origin になる。
        self.transport.playback_origin_beat = Some(self.transport.playhead_beat.unwrap_or(0.0));
        self.send_audio(AudioCommand::Play);
        self.transport.is_playing = true;
    }

    /// プレイヘッドを `beat` に置き、「停止で戻るホーム」 (`playback_origin_beat`)
    /// も同位置へ更新し、audio engine へ SeekTo を送る。 ruler click (arrangement /
    /// piano_roll / audio_editor) と `f` キーから共通で呼ぶ唯一の seek 経路 (= 「停止 =
    /// 最後に意図的に置いた位置に戻る」 の SSoT)。 再生中でも home を更新するので、
    /// 再生中に置き直して停止すると新しい位置へ戻る。 `beat` は呼び出し側で snap 済を渡す。
    pub(crate) fn seek_playhead_to(&mut self, beat: f64) {
        let beat = beat.max(0.0);
        self.transport.playhead_beat = Some(beat as f32);
        self.transport.playback_origin_beat = Some(beat as f32);
        let sr = self.ipc.sample_rate as f64;
        let bpm = self.song_doc.song().bpm.max(1.0) as f64;
        let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;
        // ensure-synced: beat→sample 換算は song.bpm を使う。 直前の BPM 編集が
        // 未 flush だと engine の再生グリッドが旧 bpm のままで seek 位置がずれる。
        // epoch 未変化なら no-op。
        self.flush_song_sync();
        self.send_audio(AudioCommand::SeekTo { samples });
    }

    /// `f` キーの実体。 snap 済 song-absolute beat へプレイヘッドを置き
    /// (`seek_playhead_to`: home も更新 + SeekTo)、停止中は `play()` を呼んでその位置から
    /// 再生開始する (play() の export / asset / plugin ゲートと playback_origin_beat capture を
    /// 継承するため body を再実装しない)。 再生中は `play()`/`stop()` を呼ばずシームレスに
    /// 継続する (home は `seek_playhead_to` が更新済なので Stop はこの位置へ戻る)。
    pub(crate) fn action_play_from_cursor(&mut self, beat: f64) {
        self.seek_playhead_to(beat);
        if !self.transport.is_playing {
            self.play();
        }
    }

    /// A7: register a `device_id` we are about to send `SetSlotPlugin`
    /// for, and — if playback is currently running — pause it until the
    /// last `SlotPluginLoaded` arrives. Without the pause, plugins loaded
    /// while playing render silent until the audio engine's
    /// `OpenPluginShmem` register catches up (typically several buffers
    /// or a loop wrap behind).
    ///
    /// v29: 要求 generation を採番して返す (呼び出し側は `SetSlotPlugin`
    /// に載せる)。 応答 (`SlotPluginLoaded` / `SlotPluginLoadFailed`) は
    /// この generation と一致するものだけ受理される (stale 応答 race guard)。
    pub(crate) fn track_pending_load(&mut self, device_id: u64) -> u64 {
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.is_playing {
            self.send_audio(AudioCommand::Stop);
            self.transport.is_playing = false;
            self.transport.pending_play = true;
        }
        self.ipc.next_plugin_load_generation = self.ipc.next_plugin_load_generation.wrapping_add(1).max(1);
        let generation = self.ipc.next_plugin_load_generation;
        self.ipc.pending_plugin_loads.insert(device_id, generation);
        if self.transport.pending_play {
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
        }
        generation
    }

    pub(crate) fn stop(&mut self) {
        self.send_audio(AudioCommand::Stop);
        self.transport.is_playing = false;
        // Pro Tools 流: 停止時に playhead を「再生開始位置」 (= 直前の
        // play() 呼び出し時点の playhead) に戻す。 GUI 側 playhead_beat
        // の即時上書きと、 audio engine への SeekTo IPC を 1 セットで
        // 実行する。 後者を送らないと on_tick が直近サンプル位置を返し
        // て GUI 側の戻し操作を打ち消す。 origin が None (= まだ一度も
        // play していない) なら playhead は触らない。
        if let Some(origin) = self.transport.playback_origin_beat.take() {
            self.transport.playhead_beat = Some(origin);
            let sr = self.ipc.sample_rate as f64;
            let bpm = self.song_doc.song().bpm.max(1.0) as f64;
            let samples = (origin as f64 * 60.0 / bpm * sr).max(0.0) as u64;
            // ensure-synced: SeekTo の beat→sample は song.bpm 依存 (seek_playhead_to
            // と同旨)。 epoch 未変化なら no-op。
            self.flush_song_sync();
            self.send_audio(AudioCommand::SeekTo { samples });
        }
        // Phase 4 Step C: recording session を transport stop でクローズ。
        // Latch / Write の latched set + per-param 直近 record 位置を全て
        // clear。 これで次の Play 時には latched / last_beat が空からスタート、
        // touching しない limit 何も record されない (Touch / Latch / Write 共通)。
        self.recording.latched_param_gestures.clear();
        self.recording.recording_last_beat.clear();
        // Phase 4 Step C-2: audio thread の recording bypass を解除 +
        // 最新 song を送る (= curve eval に戻る瞬間に正しい point sequence
        // が反映される)。 currently_recording_lanes は !is_playing なので
        // 必ず empty に解決する。
        self.sync_recording_lanes_with_audio();
    }

    /// パニック — 鳴っている全ての音を即座に止める。
    ///
    /// 1. 再生中なら [`Self::stop`] で transport を止める（sequencer note-off を
    ///    flush、audio clip / metronome を停止、playhead を開始位置へ戻す）。
    /// 2. 全 plugin を `ReinitAllPlugins`（deactivate→activate）で再初期化し、
    ///    note-off を無視する音源（VCV Rack 2 の hold voice）/ reverb tail /
    ///    鍵盤プレビューの stuck note / 自己発振まで確実に黙らせる。WAV 書き出し
    ///    開始時のクリーンリセットと同じ機構をそのまま流用する（ユーザー要望）。
    ///
    /// 書き出し中（offline render / 映像）は freewheel を壊さないよう no-op。
    /// reinit は fire-and-forget — 返信の `PluginsReinitDone` は pending_export が
    /// 無いので handler 側で無視される。
    ///
    /// クリック対策: `ReinitAllPlugins` は全 plugin を audio engine の mix から
    /// 一瞬で外すので、 master がフル音量のまま外すと段差クリック（「ビープ」）に
    /// なる。 そこで:
    /// 1. まず engine に `Panic` を送って master を declick フェードアウト →
    ///    **ミュート保持** させる。
    /// 2. reinit を [`PANIC_REINIT_DELAY`] だけ遅延させ、 plugin の detach が
    ///    master ミュート後に起きるようにする（`on_tick` が遅延 reinit を発火）。
    /// 3. reinit 完了通知 `PluginsReinitDone` を受けたら engine に `PanicRelease`
    ///    を送り、 master をフェードインで戻す（`panic_release_pending`）。
    ///
    /// ミュート解除を固定タイマーでなく**実際の reinit 完了**に結びつけることで、
    /// GUI メインスレッド stall や巨大 reinit でも、 plugin が mix に残ったまま
    /// master が戻る（クリック / reverb tail 復活）ことを防ぐ。engine 側にも
    /// plugin-host hang 用の安全 auto-release がある。
    pub(crate) fn panic(&mut self) {
        if self.transport.pending_video_export.is_some() || self.transport.export_stage.is_some() {
            return;
        }
        if self.transport.is_playing {
            self.stop();
        }
        self.send_audio(AudioCommand::Panic);
        self.transport.panic_reinit_due = Some(std::time::Instant::now());
        self.transport.panic_release_pending = true;
        self.ui_ephemeral.status_message = "パニック: 全ての音を停止しました".into();
    }

    pub(crate) fn toggle_loop(&mut self) {
        self.transport.is_looping = !self.transport.is_looping;
        self.send_audio(AudioCommand::SetLoop(self.transport.is_looping));
    }

    pub(crate) fn set_loop_range(&mut self, start: f64, end: f64) {
        let (start, end) = if end > start {
            (start.max(0.0), end.max(0.0))
        } else {
            (0.0, 0.0)
        };
        self.edit_song(|song| song.loop_start_beat = start);
        self.edit_song(|song| song.loop_end_beat = end);
    }

    /// `R` キー: 選択素材の bounding range (= 最小 `start_beat` 〜 最大
    /// `start_beat + length_beats`) を loop 範囲に設定し、 loop ON + 再生開始。
    /// 既に loop ON かつ現在の loop 範囲が同じ bounding range と一致するなら
    /// loop を OFF にする (再生は維持)。
    ///
    /// 対象面 (`automation`) は root の `edit_surface` arbiter が解決した結果。
    /// その面の選択素材の bounding span を `arrange_selection_beat_span` で取る。
    /// 解決できなければ no-op。
    pub(crate) fn loop_selected_clip_toggle(&mut self, automation: bool) {
        let Some((start, end)) = self.arrange_selection_beat_span(automation) else {
            return;
        };

        const EPS: f64 = 1e-9;
        let same_range = (self.song_doc.song().loop_start_beat - start).abs() < EPS
            && (self.song_doc.song().loop_end_beat - end).abs() < EPS;

        if self.transport.is_looping && same_range {
            self.transport.is_looping = false;
            self.send_audio(AudioCommand::SetLoop(false));
            return;
        }

        self.set_loop_range(start, end);
        if !self.transport.is_looping {
            self.transport.is_looping = true;
            self.send_audio(AudioCommand::SetLoop(true));
        }
        if !self.transport.is_playing {
            self.play();
        }
    }

}
