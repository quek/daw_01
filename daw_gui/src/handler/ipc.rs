//! handler::ipc — 子プロセス (daw_audio / daw_plugin_host) からの protocol event を
//! 直接 dispatch する。
//!
//! 旧構造では `main.rs` / `script.rs` が `AudioEvent` / `PluginEvent` を
//! `AppEvent::*FromChild` へ 1:1 変換していた (bridge match)。 これを廃し、
//! `AppEvent::Audio(AudioEvent)` / `AppEvent::Plugin(PluginEvent)` へ丸ごと包む
//! direct-wrap に統一した。 protocol variant ごとの「既存処理への接続」はここが
//! 一手に担う (意味は旧 bridge + handle_event arm と同一)。

use crate::app_types::*;
use crate::event::AppEvent;
use crate::state::*;
use common::protocol::{AudioCommand, AudioEvent, ChildKind, PluginCommand, PluginEvent};

impl AppData {
    /// daw_audio の `AudioEvent` を既存ハンドラへ接続する (旧 `audio_event_to_app`
    /// + 対応 handle_event arm)。
    pub(crate) fn dispatch_audio_event(&mut self, ev: AudioEvent) {
        match ev {
            AudioEvent::Hello { .. } => {}
            AudioEvent::ChildDisconnected => {
                self.handle_child_disconnected(ChildKind::Audio);
            }
            AudioEvent::ExportWavComplete { error, cancelled } => {
                // この完了が今 track している音声 render のものでなければ無視する
                // (BounceClipFxComplete の stale ガードと対称)。crash / watchdog で
                // 既に abort 済みの後着完了 (= 中止 status を「完了」で上書きしてしまう)
                // や、daw_audio の二重起動ガードが弾いた reject 完了が、走行中 export の
                // overlay / plugin render mode / status を壊すのを防ぐ。正規完了は
                // 標準 WAV / video 前段とも export_stage=AudioRender なので素通りする。
                if !matches!(self.transport.export_stage, Some(ExportStage::AudioRender { .. }))
                    && self.transport.pending_video_export.is_none()
                {
                    tracing::warn!(
                        ?error,
                        cancelled,
                        "ExportWavComplete with no active audio export; ignoring"
                    );
                    return;
                }
                // Either way, hand the plugins back to realtime mode
                // (we set Offline before triggering the export).
                self.send_plugin(PluginCommand::SetRenderMode(
                    common::protocol::RenderMode::Realtime,
                ));
                // 音声 freewheel フェーズ終了。overlay の AudioRender 状態を
                // 必ずクリアする（標準 WAV はこれで overlay が閉じ、video 後段は
                // この後 `action_export_mp4` が VideoRender を再設定する）。
                // watchdog 用の進捗タイムスタンプも落とす。
                self.transport.export_progress_at = None;
                self.transport.export_stage = None;
                if let Some(mp4_path) = self.transport.pending_video_export.take() {
                    // 音声と同じ拍範囲で video を render する (= 全曲
                    // なら None)。 取り出して消費。
                    let range_beats = self.transport.pending_video_export_range.take();
                    // picker で選んだ出力解像度 / fps の per-export
                    // override。 None (= 旧経路) なら action_export_mp4 が
                    // プロジェクト値にフォールバックする。
                    let dims = self.transport.pending_video_export_dims.take();
                    if cancelled {
                        // 前段（音声）でキャンセル → video export 全体を中止し、
                        // 映像 render には進まない。
                        if let Some(t) = self.transport.export_temp_wav.take() {
                            let _ = std::fs::remove_file(&t);
                        }
                        let _ = (mp4_path, range_beats, dims);
                        self.ui_ephemeral.status_message = "Video export をキャンセルしました".into();
                    } else {
                        // 1 ステップ video export の音声レンダリング完了 → video
                        // export を開始（音声失敗時は映像のみで続行）。
                        let wav = match &error {
                            Some(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "audio render for video export failed; video-only"
                                );
                                self.ui_ephemeral.status_message = format!(
                                    "音声レンダリング失敗 ({err}); 映像のみで書き出します"
                                );
                                if let Some(t) = self.transport.export_temp_wav.take() {
                                    let _ = std::fs::remove_file(&t);
                                }
                                None
                            }
                            None => self.transport.export_temp_wav.clone(),
                        };
                        #[cfg(windows)]
                        self.action_export_mp4(mp4_path, wav, range_beats, dims);
                        #[cfg(not(windows))]
                        let _ = (mp4_path, wav, range_beats, dims);
                    }
                } else if cancelled {
                    self.ui_ephemeral.status_message = "WAV 書き出しをキャンセルしました".into();
                } else if let Some(err) = error {
                    self.ui_ephemeral.status_message = format!("WAV 書き出し失敗: {err}");
                } else {
                    self.ui_ephemeral.status_message = "WAV 書き出し完了".to_string();
                }
            }
            AudioEvent::ExportWavProgress { done, total } => {
                // daw_audio の音声 freewheel 進捗。標準 WAV export / video 前段の
                // どちらでも来る。stage が AudioRender でない (= export 非実行 or
                // 既に映像フェーズ) なら stale とみなして無視する (overlay の
                // 亡霊化を防ぐ)。
                if matches!(self.transport.export_stage, Some(ExportStage::AudioRender { .. })) {
                    self.transport.export_stage = Some(ExportStage::AudioRender { done, total });
                    // watchdog: 進捗が来ている間は生存とみなしてタイマーをリセット。
                    self.transport.export_progress_at = Some(std::time::Instant::now());
                }
            }
            AudioEvent::BounceClipFxComplete {
                path,
                source_track,
                source_clip,
                error,
                frames,
            } => {
                self.handle_bounce_clip_fx_complete(path, source_track, source_clip, error, frames);
            }
            AudioEvent::PluginUnresponsive { device_id } => {
                // (v29 §4) dispatch timeout → 該当 device は quarantine 済み。
                // どの plugin かを可視化する (解除は respawn / 再ロード)。
                let name = self
                    .song_doc
                    .song()
                    .tracks
                    .iter()
                    .flat_map(|t| t.devices.iter())
                    .chain(self.song_doc.song().master_fx_chain.iter())
                    .find(|d| d.id == device_id)
                    .map(|d| d.plugin_id.clone())
                    .unwrap_or_else(|| format!("device {device_id}"));
                self.ui_ephemeral.status_message = format!(
                    "プラグイン {name} が応答しません — 一時的にバイパスしました \
                     (plugin_host 再起動 or 再ロードで復帰)"
                );
            }
            AudioEvent::WorkerPoolStalled => {
                // (v29 §4) pool 全体の stall = plugin_host 応答不能と解釈し、
                // 既存の respawn + state restore 経路に乗せる。
                tracing::error!("worker pool stalled; treating as plugin_host death");
                self.handle_child_disconnected(common::protocol::ChildKind::PluginHost);
            }
        }
    }

    /// daw_plugin_host の `PluginEvent` を既存ハンドラへ接続する (旧
    /// `plugin_event_to_app` + 対応 handle_event arm)。
    pub(crate) fn dispatch_plugin_event(&mut self, ev: PluginEvent) {
        match ev {
            PluginEvent::Hello { .. } => {}
            PluginEvent::ChildDisconnected => {
                self.handle_child_disconnected(ChildKind::PluginHost);
            }
            PluginEvent::PluginsReinitDone => {
                // plugins are now reinitialised to a clean state —
                // fire the stashed offline export. (If nothing is pending, a
                // stray reply; ignore.)
                if let Some((path, range, write_mod_sidecar)) = self.transport.pending_export.take() {
                    self.send_audio(AudioCommand::ExportWav {
                        path,
                        range,
                        write_mod_sidecar,
                    });
                }
                // a panic's reinit just completed — release the audio
                // engine's master declick hold so it fades back in over a now
                // clean (silent) mix. Guard on `panic_reinit_due.is_none()` so a
                // rapid second panic (whose reinit is still queued for `on_tick`)
                // doesn't release early on the previous reinit's reply.
                if self.transport.panic_release_pending && self.transport.panic_reinit_due.is_none() {
                    self.transport.panic_release_pending = false;
                    self.send_audio(AudioCommand::PanicRelease);
                }
            }
            PluginEvent::VocalSynthReady { device_id } => {
                // 歌唱合成完了 (or timeout) 通知。 同時 bounce は 1 件なので
                // device_id は echo back 用。 pending があれば offline render を開始する。
                // 合成待ち中の編集で index が動いていても stable id で現在位置へ解決する。
                let _ = device_id;
                if let Some(p) = self.ipc.pending_vocal_synth_bounce.take() {
                    let resolved = self
                        .song_doc
                        .song()
                        .tracks
                        .iter()
                        .position(|t| t.id == p.track_id)
                        .and_then(|ti| {
                            self.song_doc.song().tracks[ti]
                                .clips
                                .iter()
                                .position(|c| c.id == p.clip_id)
                                .map(|ci| ClipRef {
                                    track: ti as u32,
                                    clip: ci as u32,
                                })
                        });
                    match resolved {
                        Some(target) => self.start_clip_bounce(target, p.mode),
                        None => {
                            self.ui_ephemeral.status_message =
                                "Bounce: 対象クリップが消えたため中止しました".into();
                        }
                    }
                }
            }
            PluginEvent::SlotPluginLoaded {
                device_id,
                id,
                name,
                shmem_id,
                state_load_error,
                aux_output_count,
                generation,
            } => {
                self.on_plugin_loaded_from_child(
                    device_id,
                    id,
                    name,
                    shmem_id,
                    state_load_error,
                    aux_output_count,
                    generation,
                );
            }
            PluginEvent::SlotPluginLoadFailed {
                device_id,
                plugin_id,
                reason,
                generation,
            } => {
                self.on_plugin_load_failed_from_child(device_id, plugin_id, reason, generation);
            }
            PluginEvent::SlotPluginState { .. } => {}
            PluginEvent::AllPluginStates { entries } => {
                self.on_all_states_from_child(entries);
            }
            PluginEvent::SlotGuiOpened {
                device_id,
                width,
                height,
            } => {
                self.on_gui_opened(device_id, width, height);
            }
            PluginEvent::SlotGuiClosed { device_id } => {
                self.on_gui_closed(device_id);
            }
            PluginEvent::SlotPluginUnloaded { device_id } => {
                self.on_plugin_unloaded_from_child(device_id);
            }
            PluginEvent::PluginLatencyChanged { device_id, samples } => {
                self.on_plugin_latency_changed(device_id, samples);
            }
            PluginEvent::PluginParamList {
                device_id,
                params,
                has_embedded_gui,
            } => {
                // v29: device_id → 旧 (track_id, index) 座標へ逆引きして
                // 既存の positional cache に繋ぐ (S3b で cache 自体を id 化)。
                if let Some((track, index)) = find_device_by_id(self.song_doc.song(), device_id) {
                    self.ipc.plugin_params.insert((track, index), params);
                    self.ipc.slot_has_gui.insert((track, index), has_embedded_gui);
                }
            }
            PluginEvent::PluginParamTouched {
                device_id,
                param_id,
                display_name,
            } => {
                let Some((track, index)) = find_device_by_id(self.song_doc.song(), device_id) else {
                    return; // 削除済み device の stale event
                };
                // Phase 2c: host から来る `display_name` は placeholder
                // (= "Param N")。 `plugin_params` cache から param の
                // 本来の name を引いて上書きする。
                let resolved_name = self
                    .ipc
                    .plugin_params
                    .get(&(track, index))
                    .and_then(|params| params.iter().find(|p| p.id == param_id))
                    .map(|info| info.name.clone())
                    .unwrap_or(display_name);
                let target = common::model::AutomationTarget::PluginParam {
                    device_id,
                    param_id,
                    legacy_device_index: None,
                    legacy_slot: None,
                };
                // Phase 4 Step C-3: ParamGestureBegin として同経路で active /
                // latched に反映する (= mixer knob と同 idiom)。
                self.handle_event(AppEvent::ParamGestureBegin {
                    track_id: track,
                    target,
                    display_name: resolved_name,
                });
            }
            PluginEvent::PluginParamValueChanged {
                device_id,
                param_id,
                value,
            } => {
                // Phase 4 Step C-3: plugin GUI knob の最新値を per-(track,
                // device_index, param_id) cache に保存。
                if let Some((track, index)) = find_device_by_id(self.song_doc.song(), device_id) {
                    self.ipc
                        .plugin_param_values
                        .insert((track, index, param_id), value);
                }
            }
            PluginEvent::PluginParamGestureEnd { device_id, param_id } => {
                let Some((track, _index)) = find_device_by_id(self.song_doc.song(), device_id)
                else {
                    return;
                };
                // Phase 4 Step C-3: plugin GUI knob release。 mixer の
                // ParamGestureEnd と同経路に流す。
                let target = common::model::AutomationTarget::PluginParam {
                    device_id,
                    param_id,
                    legacy_device_index: None,
                    legacy_slot: None,
                };
                self.handle_event(AppEvent::ParamGestureEnd {
                    track_id: track,
                    target,
                });
            }
            PluginEvent::VoicevoxSynthStatus {
                device_id,
                busy,
                failing,
            } => {
                self.apply_voicevox_synth_status(device_id, busy, failing);
            }
        }
    }
}
