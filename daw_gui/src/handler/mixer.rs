//! handler::mixer — master gain / track volume/pan/send / mute-solo-arm / plugin db picker
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use std::sync::{Arc};
use common::model::{InstrumentSource, MAX_TRACK_GAIN, SendMode};
use common::protocol::AudioCommand;

impl AppData {
    pub(crate) fn set_master_gain(&mut self, gain: f32) {
        // +6 dB (amp 2.0) までブースト可 — フェーダーの MeterScale 上端に一致
        // (r.md #11。 unity 上限だと 0dB より上げると即 0dB に戻っていた)。
        let clamped = gain.clamp(0.0, MAX_TRACK_GAIN);
        self.transport.master_gain = clamped;
        self.send_audio(AudioCommand::SetMasterGain(clamped));
    }

    // -------- Plugin picker -----------------------------------------------

    /// 単一デバイスチェーン (`docs/plan_linear_chain.md` §5): plugin を選ぶと、
    /// 役割を判定せず **チェーン末尾に append** する (`index = devices.len()`)。
    /// 役割は位置から導出されるので、降格 / 昇格 / セクション振り分けは不要
    /// (ユーザーが後で並び替える)。builtin VOICEVOX を挿したときだけ vocal track
    /// 化する特例 (`source = Vocal`) は維持する。
    pub(crate) fn select_plugin_from_db(&mut self, id: String, keep_open: bool, open_gui: bool) {
        // 無修飾 / Shift は選択で閉じる。 Ctrl (keep_open) は開いたまま連続追加
        // できる。
        if !keep_open {
            self.ui_ephemeral.is_plugin_picker_open = false;
        }
        let Some(db) = self.ipc.plugin_db.clone() else {
            tracing::warn!(id, "plugin_db not available");
            return;
        };
        let Some(entry) = db.find_by_id(&id) else {
            tracing::error!(id, "picked plugin id not in database");
            return;
        };
        let entry_id = entry.id.clone();
        let entry_format = entry.format;
        // 役割導出の入力 (= ports)。append する device に持たせ、LoadSong で
        // daw_audio に運ぶ (= daw_audio が DB なしに役割を導出できる SSoT)。
        let ports = port_config_of(entry);
        let is_voicevox = entry_id.as_str() == common::plugin_db::BUILTIN_ID_VOICEVOX;
        self.ensure_first_track();

        // master bus 選択時は track Vec ではなく Song.master_fx_chain を対象に
        // する (= 音源境界なしの全 audio FX、 末尾 append)。
        let track_id = match self.cursor_track_id() {
            Some(id) => id,
            None => return,
        };
        let is_master = track_id == common::model::MASTER_TRACK_ID;

        // 挿入 index = 現在のチェーン長 (= 末尾 append)。
        let dest_index = if is_master {
            self.song_doc.song().master_fx_chain.len() as u32
        } else {
            let Some(track_idx) = self.cursor_track_index() else {
                return;
            };
            self.song_doc.song().tracks[track_idx].devices.len() as u32
        };

        // 内蔵映像効果は GUI 描画パスで処理する device。plugin_host に
        // load せず (load_builtin に該当無し)、モデルへ append するだけ。engine の
        // `process_track_owned` は `slot_to_plugin_id` 未登録の index を skip し
        // (= 音声バス素通り)、append は既存 device の index をずらさないので
        // audio 側は完全に不変。param は GUI が automation/変調を評価して描画に使う。
        // v29: 新規 device の安定 id を Song allocator で採番する
        // (0 のまま送る/積むのは禁止 — id addressing の根)。
        let Some(device_id) = self.edit_song(|song| song.alloc_device_id()) else {
            return;
        };

        let is_video = ports.is_video();
        if !is_video {
            // ユーザーが手動追加した plugin は load 完了時に daw_audio 再 sync +
            // (open_gui なら) GUI 自動 open する (project-load の一斉復元はこの
            // 集合に積まれない)。 Shift (open_gui=false) でも sync は必要なので
            // 常に積み、 auto-open だけ値で分岐する。
            self.ipc.pending_added_plugin_finalize
                .insert((track_id, dest_index), open_gui);
            self.send_set_slot_plugin(track_id, device_id, &entry_id, None);
        }

        let new_device = common::model::PluginInstance {
            id: device_id,
            ..common::model::PluginInstance::with_ports(entry_id, entry_format, ports)
        };
        if is_master {
            self.edit_song(|song| song.master_fx_chain.push(new_device));
        } else if let Some(track_idx) = self.cursor_track_index() {
            self.edit_song(move |song| {
            let track = &mut song.tracks[track_idx];
            let added_transform = new_device.plugin_id == common::video_fx::TRANSFORM_ID;
            track.devices.push(new_device);
            // Transform 配置 device を刺したら group_transform を有効化
            // (resolve_track_transform は device-gate + group_transform 値。未初期化なら
            // identity 配置で no-op になり、inspector で編集を始められない)。
            if added_transform && track.group_transform.is_none() {
                track.group_transform = Some(common::model::GroupTransform::default());
            }
            // builtin VOICEVOX を挿したら vocal track 化。 旧 "+Vocal Track"
            // ボタンの役割をここに集約。 歌詞 synth の gating 自体は
            // `Track::is_voicevox_vocal()` (= device の実在) が SSoT なので、
            // この marker が無くても device さえ在れば synth は走る。 marker は
            // legacy migration (`migrate_legacy_vocal_tracks`) の入力として残す。
            // それ以外の device を挿しても既存の vocal 状態は変えない。
            if is_voicevox {
                // 声は per-clip (`Clip::speaker_id`)。 トラックは
                // 「VOICEVOX で鳴らす」 印 (unit marker) のみ持つ。
                track.source = InstrumentSource::Vocal;
            }
            });
        }
    }

    // PR-V4: 旧 VOICEVOX synth path (begin_vocal_synth /
    // finish_vocal_synth) は削除。 vocal track は builtin VOICEVOX
    // instrument plugin で再生され、 歌詞 flush は sync_vocal_metadata で
    // 自動行われる (= explicit Synth ボタンは不要)。

    /// VOICEVOX engine の lazy spawn (旧 `begin_vocal_synth` から
    /// 移植)。 sync_vocal_metadata で「vocal track が 1 つでもある」
    /// 状態が初めて発生した時に呼ばれ、 background thread で
    /// `voicevox_engine::is_running()` を確認、 未起動なら
    /// `spawn_engine` で localhost:50021 を立ち上げる。 try は 1 度
    /// だけ (`voicevox_launch_attempted` flag で抑止)、 user が手動で
    /// engine を落とした場合は手動再起動。 spawn 後の child は
    /// `JobObject` に attach するので daw_gui 終了で auto-kill される。
    pub(crate) fn ensure_voicevox_engine(&mut self) {
        if self.voicevox.voicevox_launch_attempted {
            return;
        }
        self.voicevox.voicevox_launch_attempted = true;
        let job = Arc::clone(&self.voicevox.voicevox_job);
        std::thread::spawn(move || {
            if crate::voicevox_engine::is_running() {
                return;
            }
            let Some(engine) = crate::voicevox_engine::resolve_engine_path() else {
                let cfg_hint = crate::voicevox_engine::engine_path_config_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<no localappdata>".into());
                tracing::warn!(
                    hint = %cfg_hint,
                    "VOICEVOX engine path not configured (set DAW_VOICEVOX_PATH or write the exe path to the config file)"
                );
                return;
            };
            tracing::info!(?engine, "lazy spawn VOICEVOX engine for builtin plugin");
            match crate::voicevox_engine::spawn_engine(&engine) {
                Ok(child) => {
                    if let Err(e) = job.assign_std(&child) {
                        tracing::warn!(error = ?e, "failed to attach VOICEVOX to job");
                    }
                    // child を drop しても std::process::Child は wait
                    // しない (Windows)。 JobObject 経由で auto-kill される。
                    std::mem::forget(child);
                }
                Err(e) => {
                    tracing::error!(error = ?e, ?engine, "failed to spawn VOICEVOX engine");
                }
            }
        });
        // engine が立ち上がる (or 既に起動中) のと並行して
        // /singers を取得し、 Clip Inspector の声 dropdown を埋める。
        self.spawn_fetch_singers();
        // (talk) /speakers (talk 声一覧) も取得し、 Text clip Inspector の talk 声
        // dropdown を埋める (`docs/plan_voicevox_talk.md` §4)。
        self.spawn_fetch_speakers();
    }

    // -------- Plugin DB rescan --------------------------------------------

    pub(crate) fn begin_rescan(&mut self) {
        if self.ipc.is_rescanning {
            return;
        }
        self.ipc.is_rescanning = true;
        let slot = Arc::clone(&self.ipc.rescan_result);
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || match crate::subprocess::scan_plugins() {
            Some(mut db) => {
                // VST3 / CLAP とも descriptor からは port 構成が分からない
                // (VST3 は category tag 無し、 CLAP は feature に note 出力の有無が無い)。
                // 各プラグインを使い捨て probe プロセスで起動して note in/out・audio out
                // を読み、 PluginEntry の 3 bool (capability の SSoT) を更新する。 probe
                // 失敗 / timeout は scan-time 暫定値を保持 (退行しない)。 builtin は code が
                // SSoT なので probe しない。
                let probe_idx: Vec<usize> = db
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        matches!(
                            e.format,
                            common::plugin_format::PluginFormat::Vst3
                                | common::plugin_format::PluginFormat::Clap
                        )
                    })
                    .map(|(i, _)| i)
                    .collect();
                let total = probe_idx.len();
                for (n, &i) in probe_idx.iter().enumerate() {
                    proxy.send(AppEvent::RescanProgress { done: n, total });
                    let (format, path, id) = {
                        let e = &db.entries[i];
                        (e.format, e.path.clone(), e.id.clone())
                    };
                    if let Some(cfg) = crate::subprocess::probe_plugin_ports(format, &path, &id) {
                        let e = &mut db.entries[i];
                        e.has_note_input = cfg.has_note_input;
                        e.has_note_output = cfg.has_note_output;
                        e.has_audio_output = cfg.has_audio_output;
                        e.has_audio_input = cfg.has_audio_input;
                    }
                }
                if total > 0 {
                    proxy.send(AppEvent::RescanProgress { done: total, total });
                }
                // probe 済みを示す版を立てる (起動時の自動再 probe 判定用)。
                db.port_probe_version = common::plugin_db::PORT_PROBE_VERSION;
                if let Some(cache) = common::plugin_db::default_cache_path()
                    && let Err(e) = db.save_to_file(&cache)
                {
                    tracing::warn!(
                        error = ?e,
                        path = %cache.display(),
                        "failed to persist rescanned plugin_db"
                    );
                }
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(db);
                }
                proxy.send(AppEvent::PluginDbRescanCompleted);
            }
            None => {
                tracing::warn!("plugin rescan subprocess failed; keeping current plugin DB");
                proxy.send(AppEvent::PluginDbRescanCompleted);
            }
        });
    }

    pub(crate) fn finish_rescan(&mut self) {
        self.ipc.is_rescanning = false;
        // 走査進捗 overlay を消す (Phase B)。
        self.media.load_progress = None;
        let Some(new_db) = self.ipc.rescan_result.lock().ok().and_then(|mut g| g.take()) else {
            return;
        };
        let new_db = Arc::new(new_db);
        self.ipc.plugin_db = Some(new_db);
        self.rebuild_picker_entries();
        self.refresh_picker_visible();
        // (r.md #5 ARA2) A rescan can reclassify an already-loaded plug-in as
        // ARA-capable (e.g. a cache that predated ARA detection). Re-resolve ARA
        // documents so such a plug-in gets its `SetupAraDocument` now instead of
        // only on the next song edit.
        self.sync_ara_documents();
    }

    // -------- Mixer --------------------------------------------------------

    // Phase 6 review (SSOT fix): `track_id` は stable な Track::id。 旧 GUI
    // 側は Vec index を受け取って `self.song_doc.song().tracks.get_mut(idx)` していたが、
    // IPC を通すと audio engine 側の Vec 順序とずれて race を起こすため、
    // ここから IPC まで一貫して id で識別する。
    pub(crate) fn set_track_volume(&mut self, track_id: u32, volume: f32) {
        // +6 dB (amp 2.0) まで許可 — フェーダー / automation の range に一致
        // (r.md #11。 unity 上限だとフェーダーを 0dB より上げると 0dB へ戻った)。
        let v = volume.clamp(0.0, MAX_TRACK_GAIN);
        // 存在しない track は no-op (audio send / last-touched も出さない = 旧 early return)。
        if !self.song_doc.song().tracks.iter().any(|t| t.id == track_id) {
            return;
        }
        // SetSongBpmFromScrub と同 idiom: 値が実際に変わったときだけ dirty を立てて
        // autosave に乗せる (= edit_song_checked が changed のときだけ epoch を bump、
        // drag 途中 crash でも保存)。
        self.edit_song_checked(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let changed = (t.volume - v).abs() > f32::EPSILON;
            t.volume = v;
            changed
        });
        let msg = AudioCommand::SetTrackVolume { track: track_id, volume: v };
        self.send_audio(msg);
        // gui_01 #028 §7.3: knob 操作で last-touched param を更新。
        // `A` キー shortcut の source になる。
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ),
            display_name: "Volume".to_string(),
            touched_at: std::time::Instant::now(),
        });
    }

    pub(crate) fn set_track_pan(&mut self, track_id: u32, pan: f32) {
        let p = pan.clamp(-1.0, 1.0);
        // 存在しない track は no-op (audio send / last-touched も出さない = 旧 early return)。
        if !self.song_doc.song().tracks.iter().any(|t| t.id == track_id) {
            return;
        }
        self.edit_song_checked(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let changed = (t.pan - p).abs() > f32::EPSILON;
            t.pan = p;
            changed
        });
        let msg = AudioCommand::SetTrackPan { track: track_id, pan: p };
        self.send_audio(msg);
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Pan,
            ),
            display_name: "Pan".to_string(),
            touched_at: std::time::Instant::now(),
        });
    }

    // -------- Aux send / return -------------------------------------------

    /// Ableton "Add Return" 相当。 master 直下の通常 track を 1 本作って
    /// `"Return N"` と命名し、 track が選択中ならその track に新リターン宛て
    /// の send を 1 本足して即座に効果が聞こえるようにする。 構造変化なので
    /// `flush_song_sync` で full-song resend (= schedule 再 compile)。
    /// `action_add_instrument_track` を mirror した構成。
    pub(crate) fn action_add_return_track(&mut self) {
        // 既存リターン数 + 1 で命名 (= 派生集合の cardinality)。
        let existing_returns = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|t| self.is_return_track(t.id))
            .count();
        let Some(id) = self.edit_song(|song| song.alloc_track_id()) else {
            return;
        };
        let track = track_with(|t| {
            t.id = id;
            t.name = format!("Return {}", existing_returns + 1);
            // リターンは master 直下に流す。
            t.parent_group_id = None;
        });
        self.edit_song(move |song| song.tracks.push(track));
        // 選択中 track があれば、 そこから新リターンへ即座に send を 1 本張る
        // (Ableton "Add Return" の即時性)。 選択が無ければ wiring だけ作って
        // ユーザーが後で「＋ Send」 で繋ぐ。 自分自身宛て (= 新リターンが
        // 選択されていた可能性) は意味が無いので除外。
        if let Some(sel_id) = self.cursor_track_id()
            && sel_id != id
        {
            self.edit_song(move |song| {
                if let Some(src) = song.tracks.iter_mut().find(|t| t.id == sel_id) {
                    // v29: 新規 send は必ず per-track allocator で安定 id を採番する。
                    let send_id = src.alloc_send_id();
                    src.sends.push(common::model::Send {
                        id: send_id,
                        dest_track_id: id,
                        gain: 1.0,
                        mode: SendMode::PostFader,
                        enabled: true,
                    });
                }
            });
        }
        self.resize_track_peak_display();
        tracing::info!(return_id = id, "added return track");
    }

    /// `src_track_id` に `dest_track_id` 宛ての send を 1 本追加。 構造変化
    /// なので full-song resend。 同宛先の重複 send は許す (= Ableton も複数
    /// 同一 return への send を別途持てる訳ではないが、 本 MVP では単純に
    /// append、 picker 側で self-cycle のみ除外)。
    pub(crate) fn add_send(&mut self, src_track_id: u32, dest_track_id: u32) {
        if src_track_id == dest_track_id {
            return;
        }
        let __applied = self.edit_song_checked(|song| {
            let Some(src) = song.tracks.iter_mut().find(|t| t.id == src_track_id) else {
                return false;
            };
            // v29: 新規 send は必ず per-track allocator で安定 id を採番する。
            let send_id = src.alloc_send_id();
            src.sends.push(common::model::Send {
                id: send_id,
                dest_track_id,
                gain: 1.0,
                mode: SendMode::PostFader,
                enabled: true,
            });
            true
        });
        if !__applied {
            return;
        }
        tracing::info!(src_track_id, dest_track_id, "added send");
    }

    /// `track_id` の `sends[send_idx]` を削除。 構造変化 → full-song resend。
    /// v29: UI からは positional index で来るので、 該当 send の安定 id に
    /// 解決してから `Song::remove_track_send(track_id, send_id)` を呼ぶ
    /// (SendGain automation lane / mod routing の除去は model 側が id で行う —
    /// 旧 `reindex_send_gain_lanes` の「後続 index 詰め」 は id 化で消滅)。
    pub(crate) fn remove_send(&mut self, track_id: u32, send_idx: usize) {
        let Some(send_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.sends.get(send_idx))
            .map(|s| s.id)
        else {
            return;
        };
        let removed = self
            .edit_song(|song| song.remove_track_send(track_id, send_id))
            .unwrap_or(false);
        if removed {
            tracing::info!(track_id, send_idx, send_id, "removed send");
        }
    }

    /// `track_id` の `sends[send_idx].mode` を設定。 tap 位置 (pre/post) は
    /// routing graph に影響するので 構造変化 → full-song resend。
    pub(crate) fn set_send_mode(&mut self, track_id: u32, send_idx: usize, mode: SendMode) {
        self.edit_song_checked(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let Some(send) = t.sends.get_mut(send_idx) else {
                return false;
            };
            if send.mode == mode {
                return false;
            }
            send.mode = mode;
            true
        });
    }

    /// `sends[send_idx].gain` を 0..2 に clamp して設定 + realtime IPC。
    /// `set_track_volume` を mirror — full-song resend しない (= drag 中の
    /// 高頻度更新を audio engine が live re-read する)。 last-touched param も
    /// 更新して `A` キーで send-gain automation lane を生やせるようにする。
    pub(crate) fn set_send_gain(&mut self, track_id: u32, send_idx: usize, gain: f32) {
        // send gain も track/master と同じ +6dB 上限 (MAX_TRACK_GAIN) を共有する
        // (r.md #11 sibling: 定数を SSoT にして ceiling を一箇所で決める)。
        let g = gain.clamp(0.0, MAX_TRACK_GAIN);
        // v29: realtime IPC / automation target は positional index でなく
        // 安定 send id でアドレスする。 track/send が無ければ no-op (旧 early return)。
        let Some(send_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.sends.get(send_idx))
            .map(|s| s.id)
        else {
            return;
        };
        // 値が変わったときだけ dirty (edit_song_checked が epoch を bump)。
        self.edit_song_checked(|song| {
            let Some(send) = song
                .tracks
                .iter_mut()
                .find(|t| t.id == track_id)
                .and_then(|t| t.sends.get_mut(send_idx))
            else {
                return false;
            };
            let changed = (send.gain - g).abs() > f32::EPSILON;
            send.gain = g;
            changed
        });
        self.send_audio(AudioCommand::SetSendGain {
            track: track_id,
            send_id,
            gain: g,
        });
        self.ui_ephemeral.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::SendGain {
                    send_id,
                    legacy_send_idx: None,
                },
            ),
            display_name: format!("Send {}", send_idx + 1),
            touched_at: std::time::Instant::now(),
        });
    }

    /// `sends[send_idx].enabled` を設定 + realtime IPC。 `set_send_gain` と
    /// 同 idiom、 full-song resend しない (= 配線は維持したまま mute)。
    pub(crate) fn set_send_enabled(&mut self, track_id: u32, send_idx: usize, enabled: bool) {
        let Some(send_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.sends.get(send_idx))
            .map(|s| s.id)
        else {
            return;
        };
        self.edit_song_checked(|song| {
            let Some(send) = song
                .tracks
                .iter_mut()
                .find(|t| t.id == track_id)
                .and_then(|t| t.sends.get_mut(send_idx))
            else {
                return false;
            };
            let changed = send.enabled != enabled;
            send.enabled = enabled;
            changed
        });
        self.send_audio(AudioCommand::SetSendEnabled {
            track: track_id,
            send_id,
            enabled,
        });
    }

    pub(crate) fn toggle_track_mute(&mut self, track_id: u32) {
        let Some(Some(muted)) = self.edit_song(|song| {
            let t = song.tracks.iter_mut().find(|t| t.id == track_id)?;
            t.muted = !t.muted;
            // toggle なので値は必ず変化する → edit_song が epoch を bump (autosave)。
            Some(t.muted)
        }) else {
            return;
        };
        let msg = AudioCommand::SetTrackMuted { track: track_id, muted };
        self.send_audio(msg);
    }

    pub(crate) fn toggle_track_solo(&mut self, track_id: u32) {
        let Some(Some(solo)) = self.edit_song(|song| {
            let t = song.tracks.iter_mut().find(|t| t.id == track_id)?;
            t.solo = !t.solo;
            // toggle なので値は必ず変化する → edit_song が epoch を bump (autosave)。
            Some(t.solo)
        }) else {
            return;
        };
        let msg = AudioCommand::SetTrackSolo { track: track_id, solo };
        self.send_audio(msg);
    }

    pub(crate) fn toggle_track_armed(&mut self, track_id: u32) {
        let Some(Some(armed)) = self.edit_song(|song| {
            let t = song.tracks.iter_mut().find(|t| t.id == track_id)?;
            t.armed = !t.armed;
            // `armed` は永続 field。 mute / solo と同じく edit_song が epoch を bump。
            Some(t.armed)
        }) else {
            return;
        };
        let msg = AudioCommand::SetTrackArmed { track: track_id, armed };
        self.send_audio(msg);
        if !armed {
            // r.md #51: arm を外した瞬間に、そのトラックで鳴らしていたモニター音を
            // 止める。 note-off はもう届かない (armed でないので送り先から外れる) ので、
            // ここで消さないと鍵盤を離しても鳴り続ける。
            let held: Vec<u8> = self
                .recording
                .monitor_notes
                .iter()
                .filter(|(t, _)| *t == track_id)
                .map(|(_, p)| *p)
                .collect();
            for pitch in held {
                self.recording.monitor_notes.remove(&(track_id, pitch));
                self.send_audio(AudioCommand::PreviewNoteOff { track_id, pitch });
            }
        }
    }

    pub(crate) fn on_track_peaks_tick(&mut self, peaks: &[(f32, f32)]) {
        const RELEASE: f32 = 0.85;
        let n = self.song_doc.song().tracks.len();
        if self.transport.track_peak_display.len() != n {
            self.transport.track_peak_display.resize(n, (0.0, 0.0));
        }
        for (i, d) in self.transport.track_peak_display.iter_mut().enumerate() {
            let (l, r) = peaks.get(i).copied().unwrap_or((0.0, 0.0));
            d.0 = common::meter::update_peak(d.0, l, RELEASE);
            d.1 = common::meter::update_peak(d.1, r, RELEASE);
        }
    }

    pub(crate) fn rebuild_picker_entries(&mut self) {
        let Some(db) = self.ipc.plugin_db.as_ref() else {
            self.ui_ephemeral.plugin_picker_entries.clear();
            return;
        };
        let mut v: Vec<PluginPickEntry> =
            db.entries.iter().map(PluginPickEntry::from_db_entry).collect();
        v.sort_by_key(|e| e.name.to_lowercase());
        self.ui_ephemeral.plugin_picker_entries = v;
    }

    pub(crate) fn refresh_picker_visible(&mut self) {
        // master bus は audio FX と **映像効果** を持てる (Wave1: master 映像
        // チェーン = master_fx_chain の video device を最終合成 1 枚に apply_chain)。master
        // 選択中は FX / Video のみ出す (instrument / midi-fx は master に挿せない)。通常
        // トラックは全カテゴリ混合で見せ、種別は選択時に features から自動振り分け。
        // Transform 配置 device は master には出さない (master は全画面 = 配置の意味が薄く、
        // master group_transform の受け皿も無い)。
        let master = self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID);
        // 検索クエリ (前後空白を除去)。 空なら (master フィルタを除き) 全件、 非空なら
        // name / vendor のいずれかへの subsequence マッチで AND 絞り込みする。
        let query = self.ui_ephemeral.plugin_picker_query.trim();
        let visible: Vec<PluginPickEntry> = self
            .ui_ephemeral.plugin_picker_entries
            .iter()
            .filter(|e| {
                !master
                    || (matches!(e.category, PluginCategory::Fx | PluginCategory::Video)
                        // master には Transform 配置 device を出さない (全画面 master に配置は無意味)。
                        && e.id != common::video_fx::TRANSFORM_ID)
            })
            .filter(|e| {
                query.is_empty()
                    || crate::fuzzy::subsequence_match(&e.name, query)
                    || crate::fuzzy::subsequence_match(&e.vendor, query)
            })
            .cloned()
            .collect();
        self.ui_ephemeral.plugin_picker_visible = visible;
        // 絞り込み再計算後はカーソルを先頭に戻す (要件 7)。 query 変更 / target 切替 /
        // rescan 完了で呼ばれるため、 「絞り込みが変わったら先頭にリセット」 が自然。
        self.ui_ephemeral.plugin_picker_cursor = 0;
    }

    pub(crate) fn resolve_name(&self, plugin_id: &str) -> String {
        self.ipc.plugin_db
            .as_deref()
            .and_then(|db| db.find_by_id(plugin_id))
            .map(|e| {
                if e.name.is_empty() {
                    plugin_id.to_string()
                } else {
                    e.name.clone()
                }
            })
            .unwrap_or_else(|| plugin_id.to_string())
    }

}
