//! handler::tick — 再生 tick (playhead/meter) + automation 録音 + bpm/timesig commit
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::protocol::{AudioCommand, PluginCommand};

impl AppData {
    // -------- Tick / metering ----------------------------------------------

    pub(crate) fn on_tick(&mut self, playhead_samples: u64, peak_l_raw: f32, peak_r_raw: f32) {
        // パニックの遅延 reinit を発火する。 master の declick フェード
        // アウトが終わった頃 (`PANIC_REINIT_DELAY` 経過) に `ReinitAllPlugins` を
        // 送ることで、 plugin を mix から外す detach が master ミュート後に起き、
        // 段差クリック (「ビープ」) を出さずに reverb tail / 全 plugin 状態をクリア
        // する (`Self::panic` 参照)。
        if let Some(due) = self.transport.panic_reinit_due
            && due.elapsed() >= PANIC_REINIT_DELAY
        {
            self.transport.panic_reinit_due = None;
            self.send_plugin(PluginCommand::ReinitAllPlugins);
        }

        // Export watchdog: daw_audio が crash でなく hang した場合 (進捗 heartbeat も
        // 完了通知も止まる) は ChildDisconnected も発火しないので overlay + 入力 gate
        // が永久に残る。一定時間進捗が来なければ強制終了して脱出口を確保する。
        // daw_audio は render 中 250ms ごとに heartbeat を送るので、無進捗が
        // この閾値を超えるのは実質 hang のみ (長尺 render での誤発火は無い)。
        // VideoRender は daw_gui 内で必ず ExportFinished を返すので対象外。
        const EXPORT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(60);
        if matches!(self.transport.export_stage, Some(ExportStage::AudioRender { .. }))
            && let Some(since) = self.transport.export_progress_at
            && since.elapsed() > EXPORT_WATCHDOG
        {
            tracing::error!(
                elapsed_s = since.elapsed().as_secs(),
                "offline WAV render stalled past watchdog timeout; aborting export"
            );
            self.abort_audio_export(
                "音声エンジンが応答しないため書き出しを中止しました".into(),
            );
        }
        // plugin host が crash でなく hang した場合 (プロセス・パイプは
        // 生存のまま state_save 等で停止) は ChildDisconnected も発火せず、
        // RequestAllStates の応答が永久に来ない。 すると pending_state_queue が
        // drain せず保存 / New / Open / Open Recent / 終了(✕) が恒久ロックする
        // (#63 のダーティーガードが round-trip 完了を待つため)。 export watchdog と
        // 同型に、 一定時間応答が無ければ round-trip を破棄して脱出口を作る。
        self.poll_state_roundtrip_watchdog(std::time::Instant::now());
        let next_beat = if playhead_samples == u64::MAX {
            None
        } else {
            common::timing::playhead_to_beat(
                Some(self.song_doc.song()),
                self.ipc.sample_rate,
                playhead_samples,
            )
            .map(|b| b as f32)
        };
        // 曲末に達したら engine の auto-stop (engine.rs の
        // `song_ended` 判定で playing=false) に合わせて GUI 側 transport も止め、
        // 再生開始位置 (origin) へ戻す。Tick は playing 状態を運ばないので、engine
        // と同一の `song_ended` 述語 (= 停止境界がサンプル単位で一致) を使って GUI
        // 自身が検知する。これが無いと engine が曲末で止まっても is_playing が true
        // のまま playhead が末尾に固着する (既存の不整合)。手動 Stop と同じ
        // stop() を通すので「どんな止まり方でも再生を押した位置へ戻る」が一貫する。
        // loop 中は engine が wrap して止まらないので対象外。
        if self.transport.is_playing
            && !self.transport.is_looping
            && playhead_samples != u64::MAX
            && common::timing::song_ended(
                Some(self.song_doc.song()),
                self.ipc.sample_rate,
                playhead_samples,
            )
        {
            self.stop();
        }
        // 再生中のみ Tick の playhead を反映する。 停止中は GUI 側 playhead が
        // 権威 (stop() の「開始位置へ戻す」 / ruler seek / engine respawn 後の
        // 据え置き)。 これを入れないと、 stop() が playhead を origin に戻した
        // 後に IPC キューへ残った in-flight Tick (engine が Stop/SeekTo を反映
        // する前に読まれた直近サンプル位置) が後着で playhead を打ち消し、
        // 「Space で停止してもプレイヘッドが元位置に戻らないことがある」 race を
        // 生む。 stop() の SeekTo は engine 側カーソルを次の Play 用に揃えるため
        // 引き続き送る (= GUI 表示の権威と engine state を分離)。
        if self.transport.is_playing && next_beat != self.transport.playhead_beat {
            self.transport.playhead_beat = next_beat;
        }

        // 再生追従スクロール (Alt+F で off/scroll/page)。 playhead 反映直後に follow
        // mode に応じて arrange_scroll_beat を更新する。 canvas 幅は前フレーム描画値
        // (last_arrange_canvas_size.0)。 手動スクロール / ズームで follow が Off に落ちる
        // のは各 view 操作 handler 側 (cancel_follow_on_manual_view_change)。 follow が
        // 直接 arrange_scroll_beat を書く (= SetArrangeScroll event を経由しない) ので
        // 自分自身で Off に落ちることはない。
        if self.transport.is_playing
            && let Some(ph) = self.transport.playhead_beat
        {
            let visible_beats = self.ui_ephemeral.last_arrange_canvas_size.0 / self.ui_prefs.arrange_zoom_x.max(1.0);
            if let Some(new_scroll) = Self::follow_scroll_beat(
                self.ui_prefs.arrange_follow,
                ph,
                self.ui_prefs.arrange_scroll_beat,
                visible_beats,
            ) {
                self.ui_prefs.arrange_scroll_beat = new_scroll.max(0.0);
            }
        }

        // Phase 4 Step C: recording tick。 is_playing 中で recording_mode が
        // Read 以外、 かつ active ∪ latched gesture が non-empty なら、 各
        // gesture の現在 plain 値を AutomationPoint として playhead 位置に
        // 書き込む (1/64 beat throttle)。 Step C-2: audio thread は
        // `SetRecordingLanes` で受け取った set の lane の curve eval を bypass
        // しているので、 per-tick LoadSong は不要 (= recording 中は audio が
        // track.volume / track.pan の live value をそのまま鳴らす、 recording
        // 終了の瞬間に sync_recording_lanes_with_audio が LoadSong を送る)。
        //
        // `docs/plan_image_automation.md` §5: image PiP drag 中は再生
        // していなくても record path を動かす (= 停止中の drag で現
        // playhead に keyframe を打つ AE / Premiere 流 UX)。 image-only
        // 例外なので audio 経路は従来通り is_playing 必須。
        let image_dragging = self.image_pip_drag_active();
        // image drag 中は recording_mode = Read でも record path を回す
        // (= 「停止中の drag で現 playhead に keyframe」 を許可。 audio
        // 経路は recording_mode を尊重)。
        let audio_recording =
            self.transport.is_playing && self.recording.recording_mode != common::model::RecordingMode::Read;
        if (audio_recording || image_dragging)
            && let Some(ph) = self.transport.playhead_beat
        {
            let _inserted = self.record_automation_points_for_tick(f64::from(ph));
        }

        // the plugin editor's ✕ is now handled inside the
        // plugin-host process (its WNDPROC), which tears the GUI down and
        // sends `SlotGuiClosed` back. daw_gui no longer polls a local
        // close flag here.

        const RELEASE: f32 = 0.85;
        let new_l = common::meter::update_peak(self.transport.peak_l_display, peak_l_raw, RELEASE);
        let new_r = common::meter::update_peak(self.transport.peak_r_display, peak_r_raw, RELEASE);
        self.transport.peak_l_display = new_l;
        self.transport.peak_r_display = new_r;
        self.transport.peak_l_norm = common::meter::db_to_norm(common::meter::linear_to_db(new_l));
        self.transport.peak_r_norm = common::meter::db_to_norm(common::meter::linear_to_db(new_r));
    }

    /// Phase 4 Step C: tick ごとの automation recording。 `is_playing` と
    /// `recording_mode != Read` が caller で確認済の前提。 各 active ∪ latched
    /// gesture について、 該当 track に同 target を持つ lane を探し、 lane 内
    /// で playhead を含む clip を探し、 1/64 beat throttle で AutomationPoint
    /// を insert する。
    ///
    /// Touch mode は active のみ、 Latch / Write は active ∪ latched (latched は
    /// `ParamGestureBegin` 時に再生中なら自動で insert 済)。
    ///
    /// 戻り値は今 tick で insert した点の総数 (= 0 なら sync skip)。 lane / clip が
    /// 見つからない gesture は `ensure_recording_lane_clip` で自動生成してから記録する
    /// (B5 r.md #8: Bitwig 流 auto-create。 旧実装は silently skip で「録音しても無反応」)。
    pub(crate) fn record_automation_points_for_tick(&mut self, playhead_beat: f64) -> usize {
        // recording_mode = Read でも image / text PiP drag 中だけは continue
        // (= 「停止中の drag が AE/Premiere 流の auto-keyframe を打つ」
        // 仕様。 audio gesture には影響しない、 drag が active な image /
        // text lane だけが record される)。
        let visual_dragging = self.image_pip_drag_active() || self.text_pip_drag_active();
        if self.recording.recording_mode == common::model::RecordingMode::Read && !visual_dragging {
            return 0;
        }
        // active ∪ latched (Touch mode は latched が常に空なので active のみ)。
        let mut recording: Vec<(u32, common::model::AutomationTarget)> = Vec::new();
        for key in self.recording.active_param_gestures.iter() {
            recording.push(key.clone());
        }
        if matches!(
            self.recording.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) {
            for key in self.recording.latched_param_gestures.iter() {
                if !self.recording.active_param_gestures.contains(key) {
                    recording.push(key.clone());
                }
            }
        }
        if recording.is_empty() {
            return 0;
        }

        const THIN_INTERVAL_BEATS: f64 = 1.0 / 64.0;
        let mut inserted = 0usize;
        for (track_id, target) in recording {
            let last = self
                .recording.recording_last_beat
                .get(&(track_id, target.clone()))
                .copied();
            if let Some(prev) = last
                && playhead_beat - prev < THIN_INTERVAL_BEATS
            {
                continue;
            }
            // 現在 plain 値 (= live knob 位置) を取得。 TrackBuiltin は song の
            // 現在値、 PluginParam は `plugin_param_values` cache
            // (`PluginParamValueChanged` で更新) から引く (`current_plain_value`
            // 参照)。 値が無ければ skip。
            let plain_value = match self.current_plain_value(track_id, &target) {
                Some(v) => v,
                None => continue,
            };
            // lane + clip を探す。
            let (clip_start, content_id) =
                match self.find_recording_lane(track_id, &target, playhead_beat) {
                    Some(ids) => ids,
                    // B5 (r.md #8): lane / clip が無ければ自動生成して記録 (旧 silently skip)。
                    None => match self.ensure_recording_lane_clip(track_id, &target, playhead_beat) {
                        Some(ids) => ids,
                        None => continue,
                    },
                };
            // AutomationPoint は clip-local 時間で保存するので、 playhead から
            // clip.start_beat を引いて local 化する。
            let clip_local_beat = playhead_beat - clip_start;
            if self.insert_recording_point(content_id, clip_local_beat, plain_value) {
                self.recording.recording_last_beat
                    .insert((track_id, target.clone()), playhead_beat);
                inserted += 1;
            }
        }
        inserted
    }

    /// B5 (r.md #8): recording 中に対象 lane / clip が無ければ自動生成する
    /// (Bitwig 流 auto-create)。 lane が無ければ `lane_default_for_target` の値で
    /// 作り、 playhead を含む clip が無ければ playhead (floor) から曲末まで覆う clip を
    /// 1 本足す。 戻り値 = `(clip.start_beat, content_id)`。 track 不在で作れなければ None。
    pub(crate) fn ensure_recording_lane_clip(
        &mut self,
        track_id: u32,
        target: &common::model::AutomationTarget,
        playhead_beat: f64,
    ) -> Option<(f64, common::model::ContentId)> {
        use common::model::{AutomationClip, AutomationContent, AutomationLane, ClipContent};
        // r.md #8 再監査: master fx (`MASTER_TRACK_ID`) の PluginParam も `song_lanes` に
        // 記録する (master は Track ではない)。 add_automation_from_last_touched と同 class。
        let is_song_level = matches!(
            target,
            common::model::AutomationTarget::SongTempo
                | common::model::AutomationTarget::SongTimeSigNumerator
        ) || track_id == common::model::MASTER_TRACK_ID;
        // L8 (r.md #8): track 不在なら content_id を alloc する前に return する
        // (orphan AutomationContent leak を防ぐ)。 song-level は track 不要。
        if !is_song_level && self.song_doc.song().track_by_id(track_id).is_none() {
            return None;
        }
        let default_value = self.lane_default_for_target(&TouchedParam {
            track_id,
            target: target.clone(),
            display_name: String::new(),
            touched_at: std::time::Instant::now(),
        });
        let clip_start = playhead_beat.floor().max(0.0);
        // M6 (r.md #8): 既存 lane の clip 配置を content alloc の前に読む (borrow 分離)。
        // (a) clip_start を含む clip があれば **再利用** (重複 clip を作らない)、
        // (b) 無ければ clip_start より後の最近接 clip 開始 (無ければ song 末尾) までに
        //     length を制限する (前方の既存 clip と重なる clip を作らない)。
        let (reuse, next_clip_start) = {
            let lanes: &[AutomationLane] = if is_song_level {
                &self.song_doc.song().song_lanes
            } else {
                self.song_doc.song()
                    .track_by_id(track_id)
                    .map(|t| t.automation_lanes.as_slice())
                    .unwrap_or(&[])
            };
            match lanes.iter().find(|l| &l.target == target) {
                Some(l) => {
                    let reuse = l
                        .clips
                        .iter()
                        .find(|c| {
                            clip_start >= c.start_beat
                                && clip_start < c.start_beat + c.length_beats
                        })
                        .map(|c| (c.start_beat, c.content_id));
                    let next = l
                        .clips
                        .iter()
                        .map(|c| c.start_beat)
                        .filter(|&s| s > clip_start)
                        .fold(f64::INFINITY, f64::min);
                    (reuse, next)
                }
                None => (None, f64::INFINITY),
            }
        };
        if let Some(reuse) = reuse {
            return Some(reuse);
        }
        let clip_len = if next_clip_start.is_finite() {
            (next_clip_start - clip_start).max(0.0)
        } else {
            (self.song_doc.song().length_beats - clip_start).max(4.0)
        };
        // L11 (r.md #8): alloc_content で content + 表示名 "Rec" を同時登録する。
        // 旧実装は AutomationClip.name="Rec" を設定していたが、 arrangement view は
        // content_name(content_id) を描くので "Rec" が表示されなかった。
        let content_id = self.edit_song(|song| {
            song.alloc_content(
                ClipContent::Automation(AutomationContent::default()),
                "Rec".into(),
            )
        })?;
        if is_song_level {
            self.ui_prefs.master_row_automation_expanded = true;
            self.edit_song(|song| {
                if let Some(lane) = song.song_lanes.iter_mut().find(|l| &l.target == target) {
                    lane.enabled = true;
                    let cid = lane.next_clip_id;
                    lane.next_clip_id += 1;
                    lane.clips.push(AutomationClip {
                        id: cid,
                        name: "Rec".into(),
                        start_beat: clip_start,
                        length_beats: clip_len,
                        content_id,
                    });
                } else {
                    let lid = song.alloc_song_lane_id();
                    song.song_lanes.push(AutomationLane {
                        id: lid,
                        target: target.clone(),
                        default_value,
                        enabled: true,
                        visible: true,
                        height_px: 60,
                        clips: vec![AutomationClip {
                            id: 1,
                            name: "Rec".into(),
                            start_beat: clip_start,
                            length_beats: clip_len,
                            content_id,
                        }],
                        next_clip_id: 2,
                    });
                }
            });
        } else {
            self.ui_prefs.expanded_automation_tracks.insert(track_id);
            let found = self
                .edit_song(|song| {
                    let Some(track) = song.track_by_id_mut(track_id) else {
                        return false;
                    };
                    if let Some(lane) =
                        track.automation_lanes.iter_mut().find(|l| &l.target == target)
                    {
                        lane.enabled = true;
                        let cid = lane.next_clip_id;
                        lane.next_clip_id += 1;
                        lane.clips.push(AutomationClip {
                            id: cid,
                            name: "Rec".into(),
                            start_beat: clip_start,
                            length_beats: clip_len,
                            content_id,
                        });
                    } else {
                        let lid = track.alloc_lane_id();
                        track.automation_lanes.push(AutomationLane {
                            id: lid,
                            target: target.clone(),
                            default_value,
                            enabled: true,
                            visible: true,
                            height_px: 60,
                            clips: vec![AutomationClip {
                                id: 1,
                                name: "Rec".into(),
                                start_beat: clip_start,
                                length_beats: clip_len,
                                content_id,
                            }],
                            next_clip_id: 2,
                        });
                    }
                    true
                })
                .unwrap_or(false);
            if !found {
                return None;
            }
        }
        Some((clip_start, content_id))
    }

    /// Phase 4 Step C-2: GUI の currently recording set (= active ∪ latched
    /// based on mode) を計算する。 audio thread に送る IPC の payload と、
    /// `record_automation_points_for_tick` の iter source の両方で使う。
    pub(crate) fn currently_recording_lanes(
        &self,
    ) -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        let mut set: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
            std::collections::HashSet::new();
        if !self.transport.is_playing || self.recording.recording_mode == common::model::RecordingMode::Read {
            return set;
        }
        for k in &self.recording.active_param_gestures {
            set.insert(k.clone());
        }
        if matches!(
            self.recording.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) {
            for k in &self.recording.latched_param_gestures {
                set.insert(k.clone());
            }
        }
        set
    }

    /// Phase 4 Step C-2: GUI の currently recording set が前回 audio thread
    /// に送った snapshot と異なる場合、 `SetRecordingLanes` IPC を送る。 set が
    /// 縮んだ (= recording 終了した lane が出た) 場合は、 audio thread が
    /// curve eval に戻るタイミングで最新 points を反映させるため、 LoadSong
    /// も送る (= `flush_song_sync`)。
    ///
    /// 呼び出し場所:
    /// - `ParamGestureBegin` handler (set が拡大する可能性)
    /// - `ParamGestureEnd` handler (Touch mode で set が縮む)
    /// - `stop()` (Latch / Write で latched 全 clear、 set が縮む)
    /// - `SetRecordingMode(_)` handler (mode 変化で latched 寄与が変わる)
    pub(crate) fn sync_recording_lanes_with_audio(&mut self) {
        let next = self.currently_recording_lanes();
        if next == self.recording.last_sent_recording_lanes {
            return;
        }
        let lanes_vec: Vec<(u32, common::model::AutomationTarget)> =
            next.iter().cloned().collect();
        // recording lane の bypass 切替は engine の curve eval を変える。 lane 集合を
        // 送る前に最新 song (録音中に insert した points 含む) を epoch flush で先に
        // 届ける (ensure-synced): bypass 解除の瞬間に record session 中の点列で正しい
        // curve が引かれる。 epoch 未変化なら flush は no-op。
        self.flush_song_sync();
        self.send_audio(AudioCommand::SetRecordingLanes { lanes: lanes_vec });
        self.recording.last_sent_recording_lanes = next;
    }

    /// Phase 4 Step C: target に対応する現在 plain 値を返す。
    /// - `TrackBuiltin(Volume / Pan)`: Song の track field から直接
    /// - `PluginParam { slot, param_id }`: `plugin_param_values` cache (= plugin
    ///   GUI からの `PluginParamValueChangedFromChild` で更新される最新値) を
    ///   引く。 cache に entry が無い場合は `None` (= 一度も plugin GUI から
    ///   value 通知が来ていない、 record skip)
    /// - Mute / Send は M5 scope 外で `None`
    pub(crate) fn current_plain_value(
        &self,
        track_id: u32,
        target: &common::model::AutomationTarget,
    ) -> Option<f64> {
        use common::model::{ClipContent, ImageBuiltinParam};
        match target {
            // Phase 5: song-level target は track_id 無関係、 Song の現在値を返す
            common::model::AutomationTarget::SongTempo => Some(f64::from(self.song_doc.song().bpm)),
            common::model::AutomationTarget::SongTimeSigNumerator => {
                Some(f64::from(self.song_doc.song().time_sig.0))
            }
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ) => self
                .song_doc.song()
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| f64::from(t.volume)),
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Pan,
            ) => self
                .song_doc.song()
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| f64::from(t.pan)),
            common::model::AutomationTarget::PluginParam { device_id, param_id, .. } => {
                // v29: 安定 device_id → positional cache key へ逆引き。
                let (t, device_index) = find_device_by_id(self.song_doc.song(), *device_id)?;
                self.ipc.plugin_param_values
                    .get(&(t, device_index, *param_id))
                    .copied()
            }
            // Image PiP: 同 track の first image event の field 値を現在値とする
            // (`docs/plan_image_automation.md` §4)。 drag が ImageEvent.field を
            // 更新 → ここで再読み込み → record_automation_points_for_tick が
            // point を打つ、 という pipeline。
            common::model::AutomationTarget::ImageBuiltin(field) => {
                let track = self.song_doc.song().tracks.iter().find(|t| t.id == track_id)?;
                let event = track.clips.iter().find_map(|c| {
                    self.song_doc.song()
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Image(img) => img.events.first(),
                            _ => None,
                        })
                })?;
                Some(f64::from(match field {
                    ImageBuiltinParam::X => event.x,
                    ImageBuiltinParam::Y => event.y,
                    ImageBuiltinParam::W => event.w,
                    ImageBuiltinParam::H => event.h,
                    ImageBuiltinParam::Opacity => event.opacity,
                    ImageBuiltinParam::Rotation => event.rotation_radians,
                }))
            }
            // Text PiP: 同 track の first text event の field 値 (image と同
            // idiom)。 23 field 全部を返す (= color / shadow も lane に流す
            // ため)。
            common::model::AutomationTarget::TextBuiltin(field) => {
                use common::model::TextBuiltinParam as T;
                let track = self.song_doc.song().tracks.iter().find(|t| t.id == track_id)?;
                let event = track.clips.iter().find_map(|c| {
                    self.song_doc.song()
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Text(t) => t.events.first(),
                            _ => None,
                        })
                })?;
                Some(f64::from(match field {
                    T::X => event.x,
                    T::Y => event.y,
                    T::W => event.w,
                    T::H => event.h,
                    T::Opacity => event.opacity,
                    T::Rotation => event.rotation_radians,
                    T::FontSize => event.font_size_px,
                    T::FillR => event.fill_color[0],
                    T::FillG => event.fill_color[1],
                    T::FillB => event.fill_color[2],
                    T::FillA => event.fill_color[3],
                    T::OutlineR => event.outline_color[0],
                    T::OutlineG => event.outline_color[1],
                    T::OutlineB => event.outline_color[2],
                    T::OutlineA => event.outline_color[3],
                    T::OutlineWidth => event.outline_width_px,
                    T::ShadowR => event.shadow_color[0],
                    T::ShadowG => event.shadow_color[1],
                    T::ShadowB => event.shadow_color[2],
                    T::ShadowA => event.shadow_color[3],
                    T::ShadowOffsetX => event.shadow_offset_px.0,
                    T::ShadowOffsetY => event.shadow_offset_px.1,
                    T::ShadowBlur => event.shadow_blur_px,
                }))
            }
            _ => None,
        }
    }

    /// Phase 4 Step C: track の lane の中から、 同 target を持ち、 かつ playhead
    /// を含む clip を持つ lane を返す。 戻り値は `(clip.start_beat, content_id)`
    /// (clip-local 時間化に必要)。 lane が無い / clip が無い場合 `None`
    /// (= record skip)。
    pub(crate) fn find_recording_lane(
        &self,
        track_id: u32,
        target: &common::model::AutomationTarget,
        playhead_beat: f64,
    ) -> Option<(f64, common::model::ContentId)> {
        // Phase 5: SongTempo / SongTimeSigNumerator は song_lanes を参照、
        // track_id は ignore (= song-level lane は track に紐付かない)。
        // r.md #8 再監査: master fx (`MASTER_TRACK_ID`) の PluginParam lane も
        // song_lanes に居るので song-level 扱い (master は Track ではない)。
        let is_song_level = matches!(
            target,
            common::model::AutomationTarget::SongTempo
                | common::model::AutomationTarget::SongTimeSigNumerator
        ) || track_id == common::model::MASTER_TRACK_ID;
        let lane = if is_song_level {
            self.song_doc.song()
                .song_lanes
                .iter()
                .find(|l| l.enabled && l.target == *target)?
        } else {
            let track = self.song_doc.song().tracks.iter().find(|t| t.id == track_id)?;
            track
                .automation_lanes
                .iter()
                .find(|l| l.enabled && l.target == *target)?
        };
        let clip = lane.clips.iter().find(|c| {
            playhead_beat >= c.start_beat && playhead_beat < c.start_beat + c.length_beats
        })?;
        Some((clip.start_beat, clip.content_id))
    }

    /// Phase 4 Step C: 指定 content (= shared automation curve) に
    /// `(time_beat, value, Linear)` point を sort 順を保って insert する。
    /// `time_beat` は **clip-local** (caller が `playhead_beat - clip.start_beat`
    /// に変換済を渡す)。 content_id の entry が `Automation` variant でない場合は
    /// false を返す。
    ///
    /// Step D thinning は `common::automation::thin_collinear_and_insert` に
    /// 抽出 (pure fn、 unit test 付き)。 ε は plain 単位で固定 0.005
    /// (Volume 範囲 0..=2 / Pan 範囲 -1..=1 のいずれでも 0.25% 程度)。
    pub(crate) fn insert_recording_point(
        &mut self,
        content_id: common::model::ContentId,
        time_beat: f64,
        plain_value: f64,
    ) -> bool {
        const THIN_EPSILON_PLAIN: f64 = 0.005;
        // 録音 gesture 途中で crash しても、 挿入済の点が autosave に乗るよう
        // dirty を立てる (= edit が epoch を bump)。 GUI tick 経路 (= audio
        // callback でない) なので RT 制約に抵触しない。 連続する record tick は
        // AutomationRecord stream gesture で 1 undo step に squash する。
        let scope = self
            .song_doc
            .stream_scope(crate::state::StreamGesture::AutomationRecord);
        self.song_doc.edit_checked(scope, move |song| {
            let entry = song.clip_contents.entry(content_id).or_insert_with(|| {
                common::model::ClipContent::Automation(common::model::AutomationContent::default())
            });
            let points = match entry {
                common::model::ClipContent::Automation(a) => &mut a.points,
                _ => return false,
            };
            common::automation::thin_collinear_and_insert(
                points,
                time_beat,
                plain_value,
                THIN_EPSILON_PLAIN,
            );
            true
        }) == Some(true)
    }

    /// `song.bpm` を変更した後に呼ぶ共通処理。Raw audio clip を「実時間 (秒)
    /// 固定」で BPM 比にスケールし (r.md #7 — Ableton Warp-off 相当: Raw は source
    /// を元速度で鳴らすので tempo が変わるとグリッド上の拍長が変わる)、Raw clip が
    /// あって実際にスケールしたら `true` を返す。 呼び出し元は scrub-drag
    /// (`SetSongBpmFromScrub`) と MIDI CC (`SongTempo` binding) の hot path。
    /// `true` のときの host への `LoadSong` は edit_song の epoch bump を runner の
    /// frame flush (`flush_song_sync`) が 1 frame 1 回へ構造的に coalesce して送る
    /// (即時 `LoadSong` の毎 tick flood を避ける)。`LoadSong` は source 不変なので
    /// decode 再利用で軽量。Raw clip が無ければ `false` を返し、呼び出し側の軽量
    /// `SetSongBpm` に委ねる。
    pub(crate) fn rescale_raw_clips_for_bpm_change(&mut self, old_bpm: f32, new_bpm: f32) -> bool {
        self.edit_song(|song| song.rescale_raw_clips_for_bpm(old_bpm, new_bpm))
            .unwrap_or(false)
    }

    /// BPM 入力欄を Enter で commit。 parse 成功なら 1.0..=400.0 に clamp して
    /// `song.bpm` に反映、 parse 失敗なら現値を維持。 どちらも edit_text を
    /// formatted な現値 (`"{:.1}"`) に書き戻して表示を整える。
    pub(crate) fn commit_bpm_edit(&mut self) {
        if let Ok(v) = self.ui_ephemeral.bpm_edit_text.trim().parse::<f32>() {
            let clamped = v.clamp(1.0, 400.0);
            if (self.song_doc.song().bpm - clamped).abs() > f32::EPSILON {
                let old_bpm = self.song_doc.song().bpm;
                self.edit_song(|song| song.bpm = clamped);
                // Raw audio clip を秒固定スケール (r.md #7)。LoadSong は edit_song の
                // epoch bump を runner の frame flush が拾って送るので、ここは model 更新のみ。
                self.edit_song(|song| song.rescale_raw_clips_for_bpm(old_bpm, clamped));
            }
        }
        self.ui_ephemeral.bpm_edit_text = format!("{:.1}", self.song_doc.song().bpm);
    }

    /// time_sig numerator 入力欄を Enter で commit。 parse 成功なら 1..=32 に
    /// clamp、 失敗なら現値維持。 edit_text は現値の string 表現に書き戻す。
    pub(crate) fn commit_time_sig_num_edit(&mut self) {
        if let Ok(v) = self.ui_ephemeral.time_sig_num_edit_text.trim().parse::<u8>() {
            let clamped = v.clamp(1, 32);
            if self.song_doc.song().time_sig.0 != clamped {
                self.edit_song(|song| song.time_sig.0 = clamped);
            }
        }
        self.ui_ephemeral.time_sig_num_edit_text = self.song_doc.song().time_sig.0.to_string();
    }

    /// time_sig denominator dropdown で選択された値を反映。 2/4/8/16 以外は無視。
    pub(crate) fn set_song_time_sig_denominator(&mut self, den: u8) {
        if !matches!(den, 2 | 4 | 8 | 16) {
            tracing::warn!(den, "ignoring invalid time_sig denominator");
            return;
        }
        if self.song_doc.song().time_sig.1 != den {
            self.edit_song(|song| song.time_sig.1 = den);
        }
    }

    /// `self.song_doc.song()` が外部要因 (open / new / undo / redo / autosave 復元 etc.) で
    /// 差し替わった後に、 transport 入力欄の表示文字列を現値に書き戻す。
    pub(crate) fn resync_song_edit_texts(&mut self) {
        self.ui_ephemeral.bpm_edit_text = format!("{:.1}", self.song_doc.song().bpm);
        self.ui_ephemeral.time_sig_num_edit_text = self.song_doc.song().time_sig.0.to_string();
        // clip 数値 field は scrubable_number 化され専用 buffer は
        // 撤去。 共有 `clip_edit_buffer_target` だけ song 差し替え (open / new
        // / undo / redo) に追従させる。 selected_clip が image / text の場合は
        // 次フレームの view 側 target 不一致検知で正しい resync が走るため、
        // ここでは audio marker (= 非 audio なら None 化) で十分。
        match self.selected_clip_ref() {
            Some(target) => self.resync_clip_audio_event_edit_buffers(target),
            None => {
                self.ui_ephemeral.clip_edit_buffer_target = None;
            }
        }
    }

}
