//! handler::mixer — master gain / track volume/pan/send / mute-solo-arm / plugin db picker
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use crate::event_device::DeviceEvent;
use std::sync::{Arc};
use common::model::{InstrumentSource, MAX_TRACK_GAIN, SendMode};
use common::protocol::AudioCommand;

impl AppData {
    pub(crate) fn set_master_gain(&mut self, gain: f32) {
        // +6 dB (amp 2.0) までブースト可 — フェーダーの MeterScale 上端に一致
        // (r.md #11。 unity 上限だと 0dB より上げると即 0dB に戻っていた)。
        let clamped = gain.clamp(0.0, MAX_TRACK_GAIN);
        // マスター音量は **曲の一部** なので Song が SSoT (= 保存され、変えると
        // dirty が立つ)。`set_track_volume` と同じ形: 実際に変わったときだけ
        // epoch を bump し (drag 途中の crash でも autosave に乗る)、engine へは
        // 軽量 IPC で即座に届ける (LoadSong の同期待ちでフェーダーが遅れない)。
        self.edit_song_checked(|song| {
            let changed = (song.master_gain - clamped).abs() > f32::EPSILON;
            song.master_gain = clamped;
            changed
        });
        self.send_audio(AudioCommand::SetMasterGain { project: self.pk(), gain: clamped });
    }

    // -------- Plugin picker -----------------------------------------------

    /// 単一デバイスチェーン (`docs/plan_linear_chain.md` §5): plugin を選ぶと、
    /// 役割を判定せず **チェーンの既定位置** (r.md #129 Q6: 組み込みの手前) に挿す。
    /// 役割は位置から導出されるので、降格 / 昇格 / セクション振り分けは不要
    /// (ユーザーが後で並び替える)。builtin VOICEVOX を挿したときだけ vocal track
    /// 化する特例 (`source = Vocal`) は維持する。
    pub(crate) fn select_plugin_from_db(&mut self, id: String, keep_open: bool, open_gui: bool) {
        // 無修飾 / Shift は選択で閉じる。 Ctrl (keep_open) は開いたまま連続追加
        // できる。
        if !keep_open {
            self.ui_ephemeral.is_plugin_picker_open = false;
        }
        // r.md #110: 「Parallel」 は plugin ではなく container、 r.md #129 (Q8): 内蔵 4 種は daw_audio が
        // in-process で処理する device。 どちらも DB を引かず、 picker を開いた chain の既定位置 (Q6) に挿す。
        // 内蔵は Shift なし (= GUI を開く) なら Par を開く。
        let native = common::model::NativeKind::from_picker_id(&id);
        if id == common::plugin_db::PARALLEL_PICKER_ID || native.is_some() {
            self.ensure_first_track();
            let Some(track_id) = self.cursor_track_id() else { return };
            let dest = self
                .ui_ephemeral
                .plugin_picker_target
                .filter(|c| self.cur.song_doc.song().chain_devices(*c).is_some())
                .unwrap_or(common::model::ChainRef::Track(track_id));
            let ev = match native {
                Some(kind) => DeviceEvent::AddNative { chain: dest, kind, open_panel: open_gui },
                None => DeviceEvent::AddParallel { chain: dest, at: InsertAt::Default },
            };
            self.handle_event(AppEvent::Device(ev));
            return;
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
            self.cur.pipc
                .pending_added_plugin_finalize
                .insert(device_id, open_gui);
            self.send_set_slot_plugin(device_id, &entry_id, None);
        }

        let new_device = common::model::PluginInstance {
            id: device_id,
            ..common::model::PluginInstance::with_ports(entry_id, entry_format, ports)
        };
        // r.md #110: 挿入先は picker を開いた chain (`+ Plugin` の行が指す chain)。
        // 無指定 / 消えていれば cursor track の top-level 末尾。
        let dest = self
            .ui_ephemeral
            .plugin_picker_target
            .filter(|c| self.cur.song_doc.song().chain_devices(*c).is_some())
            .unwrap_or(common::model::ChainRef::Track(track_id));
        // r.md #129 (Q6): 挿す位置は組み込みの手前。closure の中 (実行時の Song) で解決する。
        if is_master {
            self.edit_song(move |song| {
                let at = song.default_insert_index(dest).unwrap_or(0);
                song.insert_device(dest, at, common::model::Device::Plugin(new_device));
            });
        } else if let Some(track_idx) = self.cursor_track_index() {
            self.edit_song(move |song| {
            let added_transform = new_device.plugin_id == common::video_fx::TRANSFORM_ID;
            let at = song.default_insert_index(dest).unwrap_or(0);
            song.insert_device(dest, at, common::model::Device::Plugin(new_device));
            let track = &mut song.tracks[track_idx];
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
        let slot = Arc::clone(&self.voicevox.spawned_engine);
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
                Ok(mut child) => {
                    // (r.md #61) handle を保持する。旧実装は `std::mem::forget`
                    // で捨てており、停止手段が Job Object の CloseHandle しか
                    // 無かった (= 終了シーケンスが engine の停止を所有できない)。
                    //
                    // **slot を取ってから attach する**。`is_running()` の HTTP
                    // タイムアウト (最大 1 秒) を待っている間に終了シーケンスが
                    // 走り切って `JobHandle::close()` まで到達していると、Job にも
                    // 入らず kill もされない孤児が残るため、その場で殺す。
                    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.shutting_down {
                        tracing::warn!(
                            "VOICEVOX engine finished spawning after shutdown began; killing it"
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    if let Err(e) = job.assign_std(&child) {
                        // Job に入れられないと backstop が効かない。孤児を作るより
                        // 起動しなかったことにする方が安全 (次回起動でやり直せる)。
                        tracing::error!(error = ?e, "failed to attach VOICEVOX to job; killing it");
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    guard.child = Some(child);
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
                    .entries()
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
                        let e = &db.entries()[i];
                        (e.format, e.path.clone(), e.id.clone())
                    };
                    if let Some(cfg) = crate::subprocess::probe_plugin_ports(format, &path, &id) {
                        db.update_entry(i, |e| apply_probed_ports(e, &cfg));
                    }
                }
                if total > 0 {
                    proxy.send(AppEvent::RescanProgress { done: total, total });
                }
                // probe 済みを示す版を立てる (起動時の自動再 probe 判定用)。
                db.set_port_probe_version(common::plugin_db::PORT_PROBE_VERSION);
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
        self.cur.media.load_progress = None;
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
    // 側は Vec index を受け取って `self.cur.song_doc.song().tracks.get_mut(idx)` していたが、
    // IPC を通すと audio engine 側の Vec 順序とずれて race を起こすため、
    // ここから IPC まで一貫して id で識別する。
    pub(crate) fn set_track_volume(&mut self, track_id: u32, volume: f32) {
        // +6 dB (amp 2.0) まで許可 — フェーダー / automation の range に一致
        // (r.md #11。 unity 上限だとフェーダーを 0dB より上げると 0dB へ戻った)。
        let v = volume.clamp(0.0, MAX_TRACK_GAIN);
        // 存在しない track は no-op (audio send / last-touched も出さない = 旧 early return)。
        if !self.cur.song_doc.song().tracks.iter().any(|t| t.id == track_id) {
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
        let msg = AudioCommand::SetTrackVolume { project: self.pk(), track: track_id, volume: v };
        self.send_audio(msg);
        // gui_01 #028 §7.3: knob 操作で last-touched param を更新。
        // `A` キー shortcut の source になる。
        self.cur.peph.last_touched_param = Some(TouchedParam {
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
        if !self.cur.song_doc.song().tracks.iter().any(|t| t.id == track_id) {
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
        let msg = AudioCommand::SetTrackPan { project: self.pk(), track: track_id, pan: p };
        self.send_audio(msg);
        self.cur.peph.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Pan,
            ),
            display_name: "Pan".to_string(),
            touched_at: std::time::Instant::now(),
        });
    }

    /// EQ / Comp セクションの開閉 (全 ch 一括)。`UiPrefs` だけを触るので
    /// dirty も Undo も動かさない (`docs/plan_channel_strip.md` §8)。
    pub(crate) fn toggle_strip_section(&mut self, section: StripSection) {
        match section {
            StripSection::Comp => {
                self.cur.view.strip_comp_open = !self.cur.view.strip_comp_open;
            }
            StripSection::Eq => self.cur.view.strip_eq_open = !self.cur.view.strip_eq_open,
        }
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
            .cur.song_doc.song()
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
            // r.md #129 (§5.9): 依存が循環する send は拒否する (循環すると master が無音になる)。
            if !song.can_add_send(src_track_id, dest_track_id) {
                return false;
            }
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
    /// (その send を狙う SendGain の automation lane / mod routing は、編集後の不変条件
    /// `Song::prune_dangling_param_targets` が同じ undo step で落とす — 旧 `reindex_send_gain_lanes` の
    /// 「後続 index 詰め」 は id 化で消滅)。
    pub(crate) fn remove_send(&mut self, track_id: u32, send_idx: usize) {
        let Some(send_id) = self
            .cur.song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.sends.get(send_idx))
            .map(|s| s.id)
        else {
            return;
        };
        let removed = self
            // r.md #89 (同件): 落とした SendGain 変調の **深さ**を指していた変調 / レーンの連鎖掃除は、
            // SongDoc の `enforce_edit_invariants` が同じ undo step で担う (r.md #129)。
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
            .cur.song_doc.song()
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
            project: self.pk(),
            track: track_id,
            send_id,
            gain: g,
        });
        self.cur.peph.last_touched_param = Some(TouchedParam {
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
            .cur.song_doc.song()
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
            project: self.pk(),
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
        let msg = AudioCommand::SetTrackMuted { project: self.pk(), track: track_id, muted };
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
        let msg = AudioCommand::SetTrackSolo { project: self.pk(), track: track_id, solo };
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
        let msg = AudioCommand::SetTrackArmed { project: self.pk(), track: track_id, armed };
        self.send_audio(msg);
        if !armed {
            // r.md #51: arm を外した瞬間に、そのトラックで鳴らしていたモニター音を
            // 止める。 note-off はもう届かない (armed でないので送り先から外れる) ので、
            // ここで消さないと鍵盤を離しても鳴り続ける。
            // 止めるのは台帳の **送った鍵盤** (r.md #130、移調込み)。
            let held: Vec<(u8, u8)> = self
                .cur.recording
                .monitor_notes
                .iter()
                .filter(|((t, _), _)| *t == track_id)
                .map(|(&(_, p), &key)| (p, key))
                .collect();
            for (pitch, key) in held {
                self.cur.recording.monitor_notes.remove(&(track_id, pitch));
                self.send_preview_off(track_id, key);
            }
        }
    }

    /// audio から届いたメーター面の 1 tick を表示用の弾道に通す (peak / 内蔵 device の GR /
    /// master Limiter の GR)。
    ///
    /// GR は engine が「buffer 内で最も深かった量」を **0 以下の dB** で publish するので、
    /// ここで正の減衰量へ反転してから peak と同じ release で 0 へ戻す (メーターが 1 buffer だけ
    /// 跳ねて消えるのを防ぐ)。`native` が `None` (seqlock が読めなかった) なら GR は前回値を保つ。
    /// `peaks` は engine が publish した `(track id, L, R)` (`None` = 読めなかった tick = 前回値を保つ)。
    /// 表示 (`track_peak_display`) は GUI の曲の並びなので、id で突き合わせて積む — engine は曲の順に
    /// 並べるので通常は同じ位置で一致し、並べ替えが engine に届く前の数フレームだけ id で引き直す。
    pub(crate) fn on_track_peaks_tick(
        &mut self,
        peaks: Option<&[(u32, f32, f32)]>,
        native: Option<&[(u64, f32)]>,
        limiter_gr_db: f32,
    ) {
        const RELEASE: f32 = 0.85;
        let t = &mut self.cur.transport;
        t.master_limiter_gr = common::meter::update_peak(t.master_limiter_gr, (-limiter_gr_db).max(0.0), RELEASE);
        if let Some(plane) = native {
            t.native_gr.update(plane, RELEASE);
        }
        let Some(peaks) = peaks else { return };
        let song = self.cur.song_doc.song();
        let t = &mut self.cur.transport;
        if t.track_peak_display.len() != song.tracks.len() {
            t.track_peak_display.resize(song.tracks.len(), (0.0, 0.0));
        }
        for (i, (track, d)) in song.tracks.iter().zip(t.track_peak_display.iter_mut()).enumerate() {
            let hit = match peaks.get(i) {
                Some(&(id, l, r)) if id == track.id => Some((l, r)),
                _ => peaks.iter().find(|p| p.0 == track.id).map(|&(_, l, r)| (l, r)),
            };
            let (l, r) = hit.unwrap_or((0.0, 0.0));
            d.0 = common::meter::update_peak(d.0, l, RELEASE);
            d.1 = common::meter::update_peak(d.1, r, RELEASE);
        }
    }

    /// r.md #129 (Q14): EQ Par の背後に描く device のスペクトラム (`SPECTRUM_BANDS` 帯の dB)。
    /// watch していない / まだ届いていない device は `None`。
    pub fn device_spectrum_db(&self, device_id: u64) -> Option<&[f32]> {
        self.cur.transport.device_spectra.get(&device_id).map(|s| &s[..])
    }

    pub(crate) fn rebuild_picker_entries(&mut self) {
        // DB が無くても内蔵 4 種と Parallel は出す (r.md #129)。
        self.ui_ephemeral.plugin_picker_entries = PluginPickEntry::build_all(self.ipc.plugin_db.as_deref());
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
        // r.md #101: 先頭の `v ` / `i ` / `f ` / `m ` は種別 (Video / Instrument /
        // FX / MIDI FX) の絞り込みで、 残りが name / vendor のクエリ。
        let (category, query) = split_picker_query(&self.ui_ephemeral.plugin_picker_query);
        let visible: Vec<PluginPickEntry> = self
            .ui_ephemeral.plugin_picker_entries
            .iter()
            .filter(|e| {
                !master
                    || (matches!(e.category, PluginCategory::Fx | PluginCategory::Video | PluginCategory::Native)
                        // master には Transform 配置 device を出さない (全画面 master に配置は無意味)。
                        && e.id != common::video_fx::TRANSFORM_ID)
            })
            .filter(|e| category.is_none_or(|c| e.category.matches_filter(c)))
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
        // 本体は free 関数 1 本 (`&AppData` を持たない view からも呼ぶため)。
        // ここで body を複製すると同じ解決規則が 2 箇所に散る。
        crate::app::resolve_plugin_name(&self.ipc.plugin_db, plugin_id)
    }

}

/// r.md #101: プラグインピッカーのクエリ先頭の種別接頭辞を剥がす。
/// `v ` = 映像効果 / `i ` = 楽器 / `f ` = FX / `m ` = MIDI FX (大文字小文字を区別しない、
/// 接頭辞の後ろは空でもよい = 種別だけで絞る)。 接頭辞が無ければ `(None, 全文)`。
/// 「v」 だけ (空白なし) は接頭辞ではなく通常の検索語 (`vocoder` の途中入力)。
/// 戻りの文字列は前後空白を除いてある (呼び側の `trim()` をここに吸収)。
/// rescan の probe が読んだ port 構成 (note in/out・audio in/out) を DB の entry に書く。映像 port は内蔵映像効果
/// だけが持つので probe の対象外 (書かない)。
fn apply_probed_ports(entry: &mut common::plugin_db::PluginEntry, cfg: &common::port_config::PortConfig) {
    entry.has_note_input = cfg.has_note_input;
    entry.has_note_output = cfg.has_note_output;
    entry.has_audio_output = cfg.has_audio_output;
    entry.has_audio_input = cfg.has_audio_input;
}

pub(crate) fn split_picker_query(query: &str) -> (Option<PluginCategory>, &str) {
    let query = query.trim_start();
    let mut chars = query.chars();
    let (Some(head), Some(' ')) = (chars.next(), chars.next()) else {
        return (None, query.trim_end());
    };
    let category = match head.to_ascii_lowercase() {
        'v' => PluginCategory::Video,
        'i' => PluginCategory::Instrument,
        'f' => PluginCategory::Fx,
        'm' => PluginCategory::MidiFx,
        _ => return (None, query.trim_end()),
    };
    (Some(category), chars.as_str().trim())
}

#[cfg(test)]
mod picker_query_tests {
    use super::{PluginCategory, split_picker_query};

    #[test]
    fn prefix_selects_category_and_rest_is_the_text_query() {
        assert_eq!(split_picker_query("v "), (Some(PluginCategory::Video), ""));
        assert_eq!(split_picker_query(" v  "), (Some(PluginCategory::Video), ""));
        assert_eq!(split_picker_query("i  vital"), (Some(PluginCategory::Instrument), "vital"));
        assert_eq!(split_picker_query("F comp"), (Some(PluginCategory::Fx), "comp"));
        assert_eq!(split_picker_query("m arp"), (Some(PluginCategory::MidiFx), "arp"));
    }

    #[test]
    fn no_prefix_without_the_space_or_with_unknown_letter() {
        assert_eq!(split_picker_query("v"), (None, "v"));
        assert_eq!(split_picker_query("vital"), (None, "vital"));
        assert_eq!(split_picker_query("x comp"), (None, "x comp"));
        assert_eq!(split_picker_query("  vital "), (None, "vital"));
        assert_eq!(split_picker_query(""), (None, ""));
    }
}
