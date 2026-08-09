//! handler::devices — plugin load/gui/chain/sidechain/parallel-out + device 削除 + state round-trip
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{InstrumentSource, Track};
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, PluginCommand, SlotState};

impl AppData {
    // -------- Plugin GUI bridge --------------------------------------------

    pub(crate) fn on_gui_opened(&mut self, _device_id: u64, _width: u32, _height: u32) {
        // the editor window is created, sized, and owned by the
        // plugin-host process. daw_gui only records open state (done in
        // `open_slot_gui` when the request is sent), so there's nothing to do
        // on the opened confirmation. Plugin-initiated resize is likewise
        // handled entirely in the plugin-host process now.
    }

    pub(crate) fn on_gui_closed(&mut self, device_id: u64) {
        // The plugin-host process tore the editor window down (user clicked
        // the window's ✕, or the plugin self-closed). Drop our open-state.
        if let Some((track, index)) = find_device_by_id(self.song_doc.song(), device_id) {
            self.ipc.open_plugin_guis.remove(&(track, index));
        }
    }

    // Args mirror the `SlotPluginLoadedFromChild` AppEvent (= the IPC
    // message's fields); bundling them into a struct would just shuffle the
    // same data, so allow the wide signature.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_plugin_loaded_from_child(
        &mut self,
        device_id: u64,
        id: String,
        _name: String,
        shmem_id: String,
        // Phase 6 review (silent corruption fix): plugin_host が saved state
        // を `state_load(&bytes)` で適用しようとして失敗したときの理由。
        // `Some(reason)` のとき plugin は default 状態で chain に居る ⇒
        // ユーザーには「設定が復元されなかった」 ことを status_message で
        // 知らせて、 必要なら再 load / preset 適用してもらう。
        state_load_error: Option<String>,
        // パラアウト (docs/plan_paraout.md): plugin が宣言した aux 出力ポート数。
        // 再構築する PluginInstance に焼き込み、インスペクタの「パラアウト展開」
        // / ルーティング行が使う。
        aux_output_count: u8,
        // v29 世代 guard: `SetSlotPlugin` に載せた要求世代の echo。
        generation: u64,
    ) {
        // v29 世代 guard (`docs/plan_arch_refactor.md` §7): 最新要求世代の
        // 応答のみ受理する。 A→B と連続差し替えしたとき、 A の stale 応答が
        // B を載せた song device を巻き戻すのを防ぐ。 entry が無い (= 既に
        // 最新世代を処理済み / disconnect で clear 済み) 場合も stale 扱い。
        match self.ipc.pending_plugin_loads.get(&device_id) {
            Some(&g) if g == generation => {}
            latest => {
                tracing::warn!(
                    device_id,
                    generation,
                    ?latest,
                    "stale SlotPluginLoaded ignored (generation guard)"
                );
                return;
            }
        }
        // v29: 安定 device_id → 旧 (track_id, index) 座標へ逆引きし、 既存の
        // positional bookkeeping / song 再構築ロジックへ繋ぐ。
        let coords = find_device_by_id(self.song_doc.song(), device_id);

        // SSoT (code review 2026-06-06): audio engine に `ProcessData` shmem を
        // 開かせる。 incoming bridge の stale clone ではなく、 respawn で
        // 差し替わる live な `self.ipc.audio_tx` から送ることで、 audio respawn
        // 後にロードした plugin の音が出なくなる bug を防ぐ。 v29: 配置は
        // daw_audio が Song から解決するので device_id + shmem_id のみ。
        self.send_audio(AudioCommand::OpenPluginShmem { device_id, shmem_id });

        let Some((track_id, index)) = coords else {
            // device が song から消えている (load 中に削除された等)。 shmem は
            // 開いたままでも engine 側は Song に居ない device を schedule しない。
            // pending だけ解放して終了。
            self.ipc.pending_plugin_loads.remove(&device_id);
            tracing::warn!(device_id, %id, "SlotPluginLoaded for a device no longer in the song");
            return;
        };
        if let Some(reason) = state_load_error {
            let msg = format!(
                "Plugin state 復元失敗 (track {track_id} device {index}, id={id}): {reason}"
            );
            tracing::error!(track = track_id, index, %id, %reason, "state_load failed (notified by plugin host)");
            self.ui_ephemeral.status_message = msg;
        }
        // device_id を track_plugin_ids に登録 (delete / ungroup 時の
        // ClosePluginShmem 先送りに使用、 use-after-free deadlock 防止)。
        let entry = self.ipc.track_plugin_ids.entry(track_id).or_default();
        if !entry.contains(&device_id) {
            entry.push(device_id);
        }
        // device index 単位での load 状態 cache。 reconcile の device-level diff
        // (Undo で同 track 内の plugin 構成が変化した場合の同期) で参照。
        self.ipc.loaded_slots.insert(
            (track_id, index),
            LoadedSlotInfo {
                device_id,
                plugin_id_str: id.clone(),
            },
        );
        self.ensure_first_track();

        // resolve ports from the plugin DB (役割導出の入力)。 不在なら既存値を
        // 引き継ぐ (= reconcile 由来の再 load で既存 instance がある場合)。
        let db_ports = self
            .ipc.plugin_db
            .as_ref()
            .and_then(|db| db.find_by_id(&id).map(port_config_of));

        // 単一デバイスチェーン: master は `master_fx_chain`、 通常 track は
        // `Track.devices` に flat な device index で reconcile する。
        // PR4.5 sidechain wiring preservation: when a plugin finishes
        // loading via SlotPluginLoaded, we replace the existing
        // PluginInstance with a fresh one carrying the resolved id +
        // saved state, but **must preserve `aux_inputs` and
        // `aux_outputs`** — otherwise wiring set by the user (or loaded
        // from a saved .daw file) gets clobbered to `Vec::new()` here,
        // which then (a) makes the inspector dropdown display "—" instead
        // of the wired source / destination track, and (b) propagates to
        // daw_audio via the next LoadSong, killing the SidechainTap /
        // ParallelOutTap in `compile_schedule`. パラアウト
        // (docs/plan_paraout.md) も sidechain と同じく再ロードで生存させる。
        // r.md #9: no-op 検出付き normalize を使う。 保存ファイルと同一な再構築
        // (= 現行バージョンで保存した project の SlotPluginLoaded) では epoch を
        // bump させず、 「開いただけで '*' が付く」 のを防ぐ。 `coords` が既に
        // Some (= device は song に在る) と保証しているので、 chain lookup は成功
        // し、 実行された (= export 中でない) なら placed=true。
        let placed = self
            .normalize_song_checked(move |song| {
                let chain: Option<&mut Vec<common::model::PluginInstance>> =
                    if track_id == common::model::MASTER_TRACK_ID {
                        Some(&mut song.master_fx_chain)
                    } else {
                        song.tracks
                            .iter_mut()
                            .find(|t| t.id == track_id)
                            .map(|t| &mut t.devices)
                    };
                let Some(chain) = chain else {
                    return false;
                };
                let i = index as usize;
                let (
                    existing_state,
                    format,
                    existing_aux,
                    existing_aux_out,
                    existing_ports,
                    existing_ara,
                    existing_send_all_keys,
                ) = chain
                    .get(i)
                    .map(|p| {
                        (
                            p.state.clone(),
                            p.format,
                            p.aux_inputs.clone(),
                            p.aux_outputs.clone(),
                            p.ports,
                            p.ara_archive.clone(),
                            p.send_all_keys_to_plugin,
                        )
                    })
                    .unwrap_or((
                        None,
                        PluginFormat::Clap,
                        Vec::new(),
                        Vec::new(),
                        Default::default(),
                        None,
                        false,
                    ));
                let inst = common::model::PluginInstance {
                    // v29: 安定 device id を必ず引き継ぐ (with_ports は sentinel 0 で
                    // 作るので、 ここで焼き込まないと以後の id addressing が全滅する)。
                    id: device_id,
                    state: existing_state,
                    aux_inputs: existing_aux,
                    aux_outputs: existing_aux_out,
                    // パラアウト: the just-loaded plugin's authoritative aux output port
                    // count (overrides whatever the DB / previous instance had).
                    aux_output_count,
                    // (r.md #5 ARA2 / #9) 既存 ARA アーカイブを温存する。 `..with_ports`
                    // は ara_archive を None に落とすので、 明示引き継ぎしないと reload
                    // の度に Melodyne 等の編集が失われ、 かつ下の no-op 判定が誤って
                    // 「変化」 に倒れて開くたび dirty 化する。
                    ara_archive: existing_ara,
                    // r.md #36: 「キーを全部プラグインに送る」 も既存値を温存する。
                    // `..with_ports` は false に落とすので、 明示引き継ぎしないと
                    // 再ロードの度にユーザー設定が消え、 かつ下の no-op 判定が誤って
                    // 「変化」 に倒れて開くだけで dirty 化する (r.md #9)。
                    send_all_keys_to_plugin: existing_send_all_keys,
                    ..common::model::PluginInstance::with_ports(
                        id,
                        format,
                        db_ports.unwrap_or(existing_ports),
                    )
                };
                // no-op 検出 (r.md #9): 再構築結果が既存と同一なら epoch を bump
                // させない (= dirty 化 / 冗長な LoadSong 再送をしない)。 内容が本当に
                // 変わったとき (旧 file の port 解決 / 手動 plugin 挿入) だけ true。
                let is_change = chain.get(i) != Some(&inst);
                if i < chain.len() {
                    chain[i] = inst;
                } else {
                    chain.push(inst);
                }
                is_change
            })
            .is_some();
        if !placed {
            // track id が Vec に無い (load 中に track 削除された等)。 master でも
            // なく該当 track も居ないので、 従来どおり finalize せず early return。
            return;
        }

        // ユーザーが手動追加した plugin の load 完了 finalize:
        // (1) daw_audio へ LoadSong を再送して新 plugin を signal path に入れる。
        //     従来この add path だけ audio 再 sync が欠落しており、 save 等 次の
        //     flush_song_sync まで signal に反映されなかった (= bug)。
        // (2) GUI 自動 open を frame loop に queue する。
        // (r.md #5 ARA2) A (re)loaded plug-in at this slot is a brand-new
        // instance with an empty ARA document, even if the slot's clip set is
        // unchanged (e.g. the user re-inserted the same ARA plug-in). The ARA
        // sync cache is keyed by device_id + clip set, so without dropping
        // the cached entry here the next sync sees "no change" and never sends
        // `SetupAraDocument` to the new instance — leaving it with no regions, so
        // it renders silence and its empty playback renderer stalls the engine.
        self.ipc.ara_doc_cache.remove(&device_id);

        // 新 plugin の audio 再 sync は edit_song の epoch bump 経由: 下の play() が
        // ensure-synced flush で Play 前に新 schedule を届け (Play 待ち再生)、 Play が
        // 無い Shift 追加 (open_gui=false) でも runner の frame flush が同 frame 末に
        // LoadSong する (旧: ここで明示 sync_song_to_plugin_host していた review 修正を
        // sync 一本化で epoch flush に移譲)。 pending_added_plugin_finalize は消費する。
        if let Some(open_gui) = self.ipc.pending_added_plugin_finalize.remove(&(track_id, index))
            && open_gui
        {
            self.ipc.gui_open_requests.push((track_id, index));
        }

        // A7: this load is done. If Play was queued waiting for the
        // last plugin to register on the audio side, fire it now.
        self.ipc.pending_plugin_loads.remove(&device_id);
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.transport.pending_play = false;
            self.ui_ephemeral.status_message.clear();
            self.play();
        } else if !self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
        }

        // PR-V3: builtin VOICEVOX が load されたら、 直後に歌詞 metadata を
        // flush して背景 synth を trigger する。 plugin_id が `loaded_slots`
        // に登録された後でないと sync_vocal_metadata が skip するため、 ここで
        // 明示呼び出し。 単一デバイスチェーン化で「instrument slot」 という
        // 区分は無いので、 全 device load 後に呼ぶ (sync_vocal_metadata 内で
        // VOICEVOX device のみ拾うので overhead は最小)。
        // r.md #27: (re)load 直後の builtin plugin は fresh (synth cache 空) なので、
        // metadata 差分キャッシュから該当 device を落として初回 flush を必ず送る
        // (= seed 合成。project 切替 / plugin 差替で device_id が再利用されても確実に
        // 再送。cache が残ったままだと「同じ metadata」判定で送信 skip → 無音になる)。
        self.voicevox.voicevox_metadata_sent.remove(&device_id);
        self.sync_vocal_metadata();
    }

    /// plugin_host で `SetSlotPlugin` が失敗した (`load_plugin` Err か
    /// `ProcessDataHandle::create` Err) 通知を受けたときの後処理。
    ///
    /// A7 の `track_pending_load` で詰めた `pending_plugin_loads` の
    /// entry が plugin_host 側で消費されないと、 「プラグイン読み込み
    /// 中...」 status のまま `pending_play` が永久に flush されない
    /// (= 再生不能) になる。 失敗 = ロード round-trip 完了 と等価
    /// 扱いで pending を解放し、 必要なら queue Play を flush する。
    ///
    /// Song の slot は touch しない: 旧 plugin が居れば継続再生、 reconcile
    /// 由来で旧無し → slot 空のまま。 ユーザーには status_message でエラー
    /// を表示するだけ。
    pub(crate) fn on_plugin_load_failed_from_child(
        &mut self,
        device_id: u64,
        plugin_id: String,
        reason: String,
        generation: u64,
    ) {
        // v29 世代 guard: loaded と対称 (最新世代の失敗のみ pending を解放)。
        match self.ipc.pending_plugin_loads.get(&device_id) {
            Some(&g) if g == generation => {}
            latest => {
                tracing::warn!(
                    device_id,
                    generation,
                    ?latest,
                    "stale SlotPluginLoadFailed ignored (generation guard)"
                );
                return;
            }
        }
        tracing::error!(
            device_id,
            %plugin_id,
            %reason,
            "plugin load failed (notified by plugin host)"
        );
        self.ipc.pending_plugin_loads.remove(&device_id);
        // 失敗を「そのセッション中ずっと無音」で終わらせない: device を
        // 「未ロード」としてインスペクタに出し、 明示的な再 load
        // (`AppEvent::ReloadDevice`) の対象にする。 自動リトライはしない
        // (plugin 側の恒常的な失敗で無限ループになる)。
        self.ipc.failed_plugin_loads.insert(device_id, reason.clone());
        // load 失敗時は finalize 予約も取り消す (stale entry が後の project-load で
        // 誤 sync / 誤 open しないように)。
        if let Some((track, index)) = find_device_by_id(self.song_doc.song(), device_id) {
            self.ipc.pending_added_plugin_finalize.remove(&(track, index));
        }
        // pending_play 解放: A7 と同じロジック (`on_plugin_loaded_from_child`
        // と対称)。 失敗で空になったタイミングで queue Play を flush する。
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.transport.pending_play = false;
            self.ui_ephemeral.status_message =
                format!("プラグイン読み込み失敗: {plugin_id} ({reason})");
            self.play();
        } else if !self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            // まだ他の load が走っているなら、 残数表示を更新しつつエラーは
            // 上書き (最新の状況をユーザーに見せる)。
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み失敗: {plugin_id} ({reason}) — 残 {}",
                self.ipc.pending_plugin_loads.len()
            );
        } else {
            // pending_play は立っていない (= 再生中じゃなかった or stop 済) ので
            // 単に status にエラーを出すだけ。
            self.ui_ephemeral.status_message =
                format!("プラグイン読み込み失敗: {plugin_id} ({reason})");
        }
    }

    /// ロードに失敗した device をユーザーの明示操作で再 load する
    /// (インスペクタの「読み込み失敗」セクションの「再読込」ボタン)。
    ///
    /// 失敗した device は plugin_host に instance が無いまま song に残るので、
    /// 放置するとそのセッション中ずっと無音で、 復旧手段が「project を開き
    /// 直す」しか無かった。 **自動リトライはしない** — plugin 側の恒常的な
    /// 失敗 (DLL 欠損 / activate 失敗) で無限ループになるため、 再試行の
    /// トリガーは常にユーザーの意思。
    ///
    /// 保存済み state (`PluginInstance.state`) 込みで送るので、 一時的な
    /// 失敗 (shmem 名衝突など) から復帰したときは音色も復元される。
    pub(crate) fn reload_device(&mut self, track_id: u32, device_index: u32) {
        let Some(inst) = device_at(self.song_doc.song(), track_id, device_index).cloned() else {
            return;
        };
        // 内蔵映像 FX は plugin_host に載らない device なので再 load の
        // 対象にならない (そもそも load 失敗も起きない)。
        if inst.ports.is_video() {
            return;
        }
        let name = self.resolve_name(&inst.plugin_id);
        if self.send_set_slot_plugin(
            track_id,
            inst.id,
            &inst.plugin_id,
            inst.state.as_deref().map(<[u8]>::to_vec),
        ) {
            self.ui_ephemeral.status_message = format!("再読込中: {name}");
        } else {
            self.ui_ephemeral.status_message =
                format!("再読込できません: {name} (プラグインが見つかりません)");
        }
    }

    /// plugin_host がこの device の `ProcessData` shmem を破棄した通知
    /// (`SlotPluginShmemReleased`)。 audio engine に `ClosePluginShmem` を
    /// 転送して stale mapping を落とす。
    ///
    /// **teardown のたびに必ず来る**ので、 replace (同 device に別 plugin を
    /// 載せ直す) でも close が新 mapping の `OpenPluginShmem` に先行する。
    /// daw_gui は plugin event を受信順に処理し、 audio 宛の送信も同じ順序で
    /// 流れるため、 「Released → Loaded」 が 「Close → Open」 に保存される。
    ///
    /// SSoT (code review 2026-06-06): stale な incoming-bridge clone では
    /// なく live `self.ipc.audio_tx` から送る (audio respawn 後に dangling
    /// shmem 参照が残るのを防ぐ)。
    pub(crate) fn on_plugin_shmem_released_from_child(&mut self, device_id: u64) {
        self.send_audio(AudioCommand::ClosePluginShmem { device_id });
    }

    /// plugin_host が plugin destroy を完了した通知を受けて、
    /// `track_plugin_ids` 等の daw_gui ローカル状態をクリーンアップする。
    /// shmem の close は必ず先行する `SlotPluginShmemReleased` が担うので
    /// ここでは送らない (SSoT — 二重送信しない)。
    pub(crate) fn on_plugin_unloaded_from_child(&mut self, device_id: u64) {
        // device が空になったので「未ロード」表示も畳む (再 load の対象は
        // song に残っている device だけ)。
        self.ipc.failed_plugin_loads.remove(&device_id);
        for entry in self.ipc.track_plugin_ids.values_mut() {
            entry.retain(|p| *p != device_id);
        }
        self.ipc.track_plugin_ids.retain(|_, v| !v.is_empty());
        // slot 単位 cache からも、 同 device_id を持つ entry を retain で外す。
        self.ipc.loaded_slots
            .retain(|_, info| info.device_id != device_id);
        // builtin VOICEVOX が外れたら合成状態 entry も消す (busy のまま残ると
        // overlay / スピナーが消えない)。plugin host の deactivate も idle を報告するが二重防御。
        self.voicevox.voicevox_synth_status.remove(&device_id);
        // r.md #27: metadata 差分キャッシュも live device に揃える (unload された
        // device の stale entry を残さない。voicevox_synth_status と対称)。
        self.voicevox.voicevox_metadata_sent.remove(&device_id);
        // PR3.3: drop the latency entry for the destroyed plugin and
        // recompute every track's total since the chain shape changed.
        self.ipc.plugin_latencies.remove(&device_id);
        self.recompute_track_latencies();
    }

    /// PR3.3: store the new per-plugin reported latency, recompute the
    /// owning track's total (sum of all its plugin latencies), and push the
    /// updated `Song` to daw_audio so `compile_schedule` regenerates the
    /// PDC delay lines.
    pub(crate) fn on_plugin_latency_changed(&mut self, device_id: u64, samples: u32) {
        self.ipc.plugin_latencies.insert(device_id, samples);
        self.recompute_track_latencies();
    }

    /// Walk every `track_plugin_ids` entry, sum the plugin latencies into the
    /// matching `Track::reported_latency_samples`, and re-`flush_song_sync`
    /// if anything changed. No-op when the totals already agree.
    ///
    /// r.md #39: 書き戻し先は `Song::reported_latency_mut` (sentinel 分岐付き)。
    /// 旧実装は `track_by_id_mut` だったので `MASTER_TRACK_ID` の合計が **黙って
    /// 捨てられ**、master fx に latency 報告プラグインを挿しても PDC に載らなかった。
    pub(crate) fn recompute_track_latencies(&mut self) {
        // Compute per-track latency totals up front (reads self.ipc only) so
        // the Song mutation can go through the `edit_song` chokepoint without
        // holding a borrow of `self.ipc`.
        let totals: Vec<(u32, u32)> = self
            .ipc.track_plugin_ids
            .iter()
            .map(|(track_id, plugin_ids)| {
                let total: u32 = plugin_ids
                    .iter()
                    .map(|pid| self.ipc.plugin_latencies.get(pid).copied().unwrap_or(0))
                    .sum();
                (*track_id, total)
            })
            .collect();
        let track_ids_with_plugins: std::collections::HashSet<u32> =
            self.ipc.track_plugin_ids.keys().copied().collect();
        self.edit_song_checked(move |song| {
            let mut changed = false;
            for (track_id, total) in totals {
                if let Some(slot) = song.reported_latency_mut(track_id)
                    && *slot != total
                {
                    *slot = total;
                    changed = true;
                }
            }
            // Tracks with no loaded plugins should report 0 — clear any stale
            // value (e.g. the last plugin in a track was just removed).
            for track in &mut song.tracks {
                if !track_ids_with_plugins.contains(&track.id)
                    && track.reported_latency_samples != 0
                {
                    track.reported_latency_samples = 0;
                    changed = true;
                }
            }
            // master fx chain も同様に空なら 0 へ戻す (最後の master plugin を外した後)。
            if !track_ids_with_plugins.contains(&common::model::MASTER_TRACK_ID)
                && song.master_reported_latency_samples != 0
            {
                song.master_reported_latency_samples = 0;
                changed = true;
            }
            changed
        });
    }

    pub(crate) fn toggle_slot_gui(&mut self, index: u32) {
        // open_plugin_guis / IPC は track_id ベース。 master 選択時は
        // cursor_track_id が MASTER_TRACK_ID を返す (Vec に居ないので index 経由
        // 不可)。
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };
        // 内蔵映像 FX は plugin window を持たない。"GUI" ボタンは
        // インスペクタ内のパラメータ調整パネルをトグルする (plugin window は開かない)。
        // Transform も同様にトグル開閉 (開くと Group Transform セクションが出る。出っぱなしにしない)。
        let device = self
            .song_doc.song()
            .fx_chain_by_track_id(track_id)
            .and_then(|chain| chain.get(index as usize));
        // 映像 FX (色補正 / Transform 等) は専用の video_fx パネル。 ただし字幕
        // (`builtin.video.subtitle`) は video device だが video_fx def を持たず、
        // 専用パラメータは Text Event セクション (= Par パネルで描画) なので、 ここで
        // 弾いて下の open_plugin_params 経路へ流す。
        if let Some(d) = device
            && d.ports.is_video()
            && d.plugin_id != common::plugin_db::SUBTITLE_ID
        {
            let key = (track_id, index);
            self.ui_ephemeral.open_plugin_params = None; // 2 種のインライン param パネルは相互排他。
            self.ui_ephemeral.open_video_fx_params = if self.ui_ephemeral.open_video_fx_params == Some(key) {
                None
            } else {
                Some(key)
            };
            return; // 映像 device は plugin window を持たない。
        }
        // 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し
        // CLAP・VST3) は editor window を開けない。 代わりにインスペクタ内の汎用
        // param パネル (`open_plugin_params`) をトグルする。 builtin は format から
        // 即断 (PluginParamList 到着前でも正しく分岐)、 外部 plugin は host の
        // `PluginParamList`(has_embedded_gui=false) 通知に従う。
        let is_builtin = device.is_some_and(|d| d.format == PluginFormat::Builtin);
        let has_embedded_gui = !is_builtin
            && self
                .ipc.slot_has_gui
                .get(&(track_id, index))
                .copied()
                .unwrap_or(true);
        if !has_embedded_gui {
            let key = (track_id, index);
            self.ui_ephemeral.open_video_fx_params = None; // 2 種のインライン param パネルは相互排他。
            self.ui_ephemeral.open_plugin_params = if self.ui_ephemeral.open_plugin_params == Some(key) {
                None
            } else {
                Some(key)
            };
            return;
        }
        // 既に開いていれば閉じる (toggle)。開いていなければ open_slot_gui で開く。
        // open 状態は open_plugin_guis (id set) で追跡。実 window は
        // plugin-host プロセスが所有するので、close は CloseSlotGui を送って
        // B 側に破棄させ、SlotGuiClosed の受信で set から除去する。
        if self.ipc.open_plugin_guis.contains(&(track_id, index)) {
            if let Some(device_id) = device_id_at(self.song_doc.song(), track_id, index) {
                self.send_plugin(PluginCommand::CloseSlotGui { device_id });
            }
            return;
        }
        self.open_slot_gui(track_id, index);
    }

    /// 指定 (track_id, device_index) のプラグイン GUI を embedded container
    /// window で開く。既に開いていれば何もしない (重複 open 防止)。Windows 専用
    /// (他 OS では no-op)。`toggle_slot_gui` (手動トグル) と plugin 追加時の自動
    /// open の両方から使う。
    pub(crate) fn open_slot_gui(&mut self, track_id: u32, index: u32) {
        #[cfg(windows)]
        {
            if self.ipc.open_plugin_guis.contains(&(track_id, index)) {
                return;
            }
            let label = if track_id == common::model::MASTER_TRACK_ID {
                self.song_doc.song()
                    .master_fx_chain
                    .get(index as usize)
                    .map(|p| format!("Master / {}", self.resolve_name(&p.plugin_id)))
                    .unwrap_or_else(|| "Master".into())
            } else {
                self.song_doc.song()
                    .tracks
                    .iter()
                    .find(|t| t.id == track_id)
                    .and_then(|t| self.slot_ref_name(t, index))
                    .unwrap_or_else(|| "(unknown)".into())
            };
            // the editor's top-level window is created by the
            // plugin-host process (so JUCE cascade sub-menus work). daw_gui
            // only records open state and passes the window title.
            //
            // We are the foreground process at this moment (the user just
            // clicked in our UI), so grant the plugin-host process the right
            // to foreground its editor window. Without this, Windows' focus-
            // steal protection refuses the plugin-host's SetForegroundWindow
            // and the editor opens hidden behind the main DAW window — and a
            // plugin that reports its size only post-attach (e.g. Analog Lab)
            // looks like it "won't open". The grant is consumed by the
            // plugin-host's next SetForegroundWindow.
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{
                    ASFW_ANY, AllowSetForegroundWindow,
                };
                let _ = AllowSetForegroundWindow(ASFW_ANY);
            }
            let Some(device_id) = device_id_at(self.song_doc.song(), track_id, index) else {
                tracing::warn!(track_id, index, "open_slot_gui: no device id at slot");
                return;
            };
            self.ipc.open_plugin_guis.insert((track_id, index));
            self.send_plugin(PluginCommand::OpenSlotGuiEmbedded {
                device_id,
                title: format!("Plugin — {label}"),
            });
            // r.md #36: 「キーを全部プラグインに送る」 の現在値を open のたびに同期する
            // (plugin-host は再起動で状態を失う / device_id は open まで意味を持たない)。
            let send_all = device_at(self.song_doc.song(), track_id, index)
                .is_some_and(|p| p.send_all_keys_to_plugin);
            self.send_plugin(PluginCommand::SetEditorSendAllKeys {
                device_id,
                enabled: send_all,
            });
        }
        #[cfg(not(windows))]
        {
            let _ = (track_id, index);
        }
    }

    /// runner の frame loop から毎フレーム呼ぶ。plugin 追加 → load 完了で queue された
    /// GUI auto-open 要求を処理する (実 window 生成 + `OpenSlotGuiEmbedded` 送出)。
    /// window 生成を handle_event ではなく frame loop に置くことで、frame loop を
    /// 回さない headless test では window を作らない。
    pub(crate) fn drain_pending_gui_opens(&mut self) {
        if self.ipc.gui_open_requests.is_empty() {
            return;
        }
        for (track_id, index) in std::mem::take(&mut self.ipc.gui_open_requests) {
            self.open_slot_gui(track_id, index);
        }
    }

    #[cfg(windows)]
    pub(crate) fn slot_ref_name(&self, track: &Track, index: u32) -> Option<String> {
        let id = track.devices.get(index as usize).map(|p| p.plugin_id.as_str())?;
        Some(self.resolve_name(id))
    }

    /// inspector chain (= `Track.devices` / `master_fx_chain` を一列で表示) の
    /// reorder。`order` は gui_01 契約 `new[i] = items[order[i]]`。
    ///
    /// 単一デバイスチェーン (`docs/plan_linear_chain.md` §5): **棄却なしの純
    /// permutation**。役割は位置から再導出されるので、能力チェック / セクション跨ぎ
    /// 検証は撤廃した (任意の並び替えを許す)。`moves: Vec<(old_index, new_index)>` を
    /// 組んで 3 プロセスの per-device bookkeeping を貼り直す。
    pub(crate) fn reorder_inspector_chain(&mut self, order: &[usize]) {
        let is_master = self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID);
        // 対象チェーン (master / track) の現在の device 列と track_id を解決。
        let (track_id, old_devices): (u32, Vec<common::model::PluginInstance>) = if is_master {
            (common::model::MASTER_TRACK_ID, self.song_doc.song().master_fx_chain.clone())
        } else {
            let Some(track_idx) = self.cursor_track_index() else {
                return;
            };
            let Some(track) = self.song_doc.song().tracks.get(track_idx) else {
                return;
            };
            (track.id, track.devices.clone())
        };
        let n = old_devices.len();
        // order の妥当性検証 (長さ一致 + 0..n の permutation)。不正なら no-op。
        if order.len() != n || n == 0 {
            return;
        }
        if order.iter().any(|&o| o >= n) {
            return;
        }
        {
            let mut seen = vec![false; n];
            for &o in order {
                if std::mem::replace(&mut seen[o], true) {
                    return; // 重複 = 不正 permutation
                }
            }
        }

        // (review) daw_gui ローカルの positional cache (loaded_slots 等) の
        // 貼り替えは song チェーンと一致している前提。 ロード失敗 / 進行中の
        // plugin が song に phantom として残ると再キーがずれるので、 song
        // チェーンが loaded_slots と完全一致 (= 全 plugin がロード済) のとき
        // だけ並び替える (不一致なら snap back)。 v29: プロセス間は device_id
        // addressing なので host/audio 側の再キーは不要になった。
        let loaded_here = self
            .ipc.loaded_slots
            .keys()
            .filter(|(t, _)| *t == track_id)
            .count();
        let fully_loaded = loaded_here == n
            && old_devices.iter().enumerate().all(|(i, inst)| {
                self.ipc.loaded_slots
                    .get(&(track_id, i as u32))
                    .is_some_and(|info| info.plugin_id_str == inst.plugin_id)
            });
        if !fully_loaded {
            self.ui_ephemeral.status_message =
                "プラグインの読み込み中または失敗のため並び替えできません".to_string();
            return;
        }

        // 新順での device 列を組む (new[i] = old[order[i]])。
        let new_devices: Vec<common::model::PluginInstance> =
            order.iter().map(|&o| old_devices[o].clone()).collect();
        // moves: 各新位置 i について (old_index, new_index) = (order[i], i)。
        let moves: Vec<(u32, u32)> =
            (0..n).map(|i| (order[i] as u32, i as u32)).collect();

        // song を書き換え。
        let applied = if is_master {
            self.edit_song(move |song| song.master_fx_chain = new_devices)
                .is_some()
        } else {
            self.edit_song_checked(move |song| {
                if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) {
                    t.devices = new_devices;
                    true
                } else {
                    false
                }
            })
        };
        if !applied {
            return;
        }

        // song を組み替えただけでは plugin host のチェーンも audio engine の
        // index→plugin_id マップも追従しない (= 見た目だけ並び替わり音は旧順の
        // まま)。 旧→新 index の `moves` で 3 プロセスの per-device bookkeeping を
        // 貼り直してから LoadSong (= schedule 再構築) を送る。
        self.apply_chain_reorder(track_id, moves);
    }

    /// re-key our own `(track, device_index)`-keyed caches after an
    /// inspector-chain reorder. `moves` is the complete
    /// `(old_index, new_index)` permutation for `track_id` (one entry
    /// per loaded plugin, `from == to` for ones that stayed put). The caller
    /// has already rewritten `self.song_doc.song()` and follows up with
    /// `flush_song_sync` (= LoadSong resend) — v29 ではそれだけで
    /// 3 プロセスの並びが揃う (host は順序を持たない)。
    pub(crate) fn apply_chain_reorder(&mut self, track_id: u32, moves: Vec<(u32, u32)>) {
        // Local caches: remove ALL old keys first (snapshot), then re-insert at
        // the new keys, so a swap (0↔1) can't clobber the second entry.
        let mut new_loaded = Vec::new();
        let mut new_open = Vec::new();
        let mut new_params = Vec::new();
        let mut new_has_gui = Vec::new();
        for &(from, to) in &moves {
            if let Some(v) = self.ipc.loaded_slots.remove(&(track_id, from)) {
                new_loaded.push((to, v));
            }
            if self.ipc.open_plugin_guis.remove(&(track_id, from)) {
                new_open.push(to);
            }
            if let Some(v) = self.ipc.plugin_params.remove(&(track_id, from)) {
                new_params.push((to, v));
            }
            if let Some(v) = self.ipc.slot_has_gui.remove(&(track_id, from)) {
                new_has_gui.push((to, v));
            }
        }
        for (to, v) in new_loaded {
            self.ipc.loaded_slots.insert((track_id, to), v);
        }
        for to in new_open {
            self.ipc.open_plugin_guis.insert((track_id, to));
        }
        for (to, v) in new_has_gui {
            self.ipc.slot_has_gui.insert((track_id, to), v);
        }
        for (to, v) in new_params {
            self.ipc.plugin_params.insert((track_id, to), v);
        }
        // v29: automation lane / mod routing / MIDI binding の PluginParam は
        // 安定 `device_id` addressing になったので、 並び替えでの remap は不要
        // (id は device と一緒に動く)。 旧 `ReorderChain` IPC も廃止 —
        // plugin_host は順序を持たず (flat HashMap<device_id, ..>)、 audio
        // engine の処理順は caller が続けて送る LoadSong (=
        // `flush_song_sync`) が Song から compile する
        // (`docs/plan_arch_refactor.md` §1)。
    }

    /// PR4 sidechain: route a track's output into a plugin's `aux_in_port`.
    /// `source = None` disconnects. The plugin's
    /// `PluginInstance.aux_inputs[port]` slot is created on demand;
    /// shorter vectors are extended with `None` placeholders so port `port`
    /// becomes addressable. After mutation we re-`flush_song_sync`
    /// so `compile_schedule` regenerates the `SidechainTap` ops.
    /// r.md #36: この device のエディタ窓で 「キーを全部プラグインに送る」 かを設定する。
    /// project に保存し (undo 対象)、 plugin-host にも即時反映する。
    pub(crate) fn set_plugin_send_all_keys(
        &mut self,
        track_id: u32,
        device_index: u32,
        enabled: bool,
    ) {
        self.edit_song_checked(|song| {
            let inst = if track_id == common::model::MASTER_TRACK_ID {
                song.master_fx_chain.get_mut(device_index as usize)
            } else {
                let Some(track) = song.track_by_id_mut(track_id) else {
                    return false;
                };
                track.devices.get_mut(device_index as usize)
            };
            let Some(inst) = inst else { return false };
            if inst.send_all_keys_to_plugin == enabled {
                return false;
            }
            inst.send_all_keys_to_plugin = enabled;
            true
        });
        if let Some(device_id) = device_id_at(self.song_doc.song(), track_id, device_index) {
            self.send_plugin(PluginCommand::SetEditorSendAllKeys { device_id, enabled });
        }
    }

    pub(crate) fn set_sidechain_source(
        &mut self,
        track_id: u32,
        device_index: u32,
        port: u8,
        source: Option<u32>,
    ) {
        // 単一デバイスチェーン: master は master_fx_chain、 通常 track は devices
        // を flat な device index で引く。
        self.edit_song_checked(|song| {
            let inst = if track_id == common::model::MASTER_TRACK_ID {
                song.master_fx_chain.get_mut(device_index as usize)
            } else {
                let Some(track) = song.track_by_id_mut(track_id) else {
                    return false;
                };
                track.devices.get_mut(device_index as usize)
            };
            let Some(inst) = inst else {
                return false;
            };
            let port_idx = port as usize;
            if inst.aux_inputs.len() <= port_idx {
                inst.aux_inputs.resize(port_idx + 1, None);
            }
            // Phase 1: UI は常に PostFader タップを張る (旧 sidechain と同挙動)。
            // Pre/PostFx トグルは Phase 6 で追加する (docs/plan_modulation.md §9)。
            inst.aux_inputs[port_idx] = source.map(common::model::AuxInputRoute::post_fader);
            true
        });
    }

    /// パラアウト (docs/plan_paraout.md): route one aux output `port` of the
    /// plugin at `(track_id, device_index)` to `dest` (or `None` = unrouted).
    /// Mirror of `set_sidechain_source` (aux_outputs instead of aux_inputs).
    /// Used by the inspector dropdown for re-adjustment; not auto-undoable
    /// (matches sidechain), but marks dirty + recompiles via
    /// `flush_song_sync`.
    pub(crate) fn set_parallel_output_route(
        &mut self,
        track_id: u32,
        device_index: u32,
        port: u8,
        dest: Option<u32>,
    ) {
        self.edit_song_checked(|song| {
            let inst = if track_id == common::model::MASTER_TRACK_ID {
                song.master_fx_chain.get_mut(device_index as usize)
            } else {
                let Some(track) = song.track_by_id_mut(track_id) else {
                    return false;
                };
                track.devices.get_mut(device_index as usize)
            };
            let Some(inst) = inst else {
                return false;
            };
            let port_idx = port as usize;
            if inst.aux_outputs.len() <= port_idx {
                inst.aux_outputs.resize(port_idx + 1, None);
            }
            inst.aux_outputs[port_idx] = dest.map(common::model::AuxOutputRoute::to_track);
            true
        });
    }

    /// パラアウト (docs/plan_paraout.md): one-click "explode" of a multi-out
    /// plugin. For each `is_main=false` output port the plugin declares, create
    /// a child track parented to the source track and wire the aux output to
    /// it. The source track thereby becomes a group-with-instrument bus: its
    /// own main signal + the children sum through its FX / fader to the master.
    /// Idempotent — a port already routed to a still-existing track is kept (so
    /// re-clicking only fills gaps, never duplicates). Undo snapshot + dirty
    /// are taken at the dispatch choke point (`is_undoable`), so this only
    /// mutates the model and syncs.
    pub(crate) fn explode_parallel_out(&mut self, track_id: u32, device_index: u32) {
        // The grouped explode model needs the source to be a real track
        // (master has no `parent_group_id` children).
        if track_id == common::model::MASTER_TRACK_ID {
            return;
        }
        let Some(src) = self.song_doc.song().track_by_id(track_id) else {
            return;
        };
        let Some(inst) = src.devices.get(device_index as usize) else {
            return;
        };
        let count = inst.aux_output_count as usize;
        if count == 0 {
            return;
        }
        let src_name = src.name.clone();
        // Snapshot the current routes + the set of live track ids so the loop
        // below can keep valid existing routes without re-borrowing `self.song_doc.song()`
        // while it allocates ids / inserts tracks.
        let existing: Vec<Option<u32>> = (0..count)
            .map(|port| {
                inst.aux_outputs
                    .get(port)
                    .and_then(|o| o.as_ref())
                    .map(|r| r.dest_track)
            })
            .collect();
        let live_ids: std::collections::HashSet<u32> =
            self.song_doc.song().tracks.iter().map(|t| t.id).collect();

        let mut routes: Vec<Option<common::model::AuxOutputRoute>> = vec![None; count];
        let mut new_children: Vec<common::model::Track> = Vec::new();
        for (port, existing_dest) in existing.iter().enumerate() {
            // Keep an already-wired route if its destination still exists.
            if let Some(dest) = existing_dest
                && live_ids.contains(dest)
            {
                routes[port] = Some(common::model::AuxOutputRoute::to_track(*dest));
                continue;
            }
            let Some(child_id) = self.edit_song(|song| song.alloc_track_id()) else {
                return;
            };
            let name = format!("{src_name} Out {}", port + 1);
            new_children.push(track_with(|t| {
                t.id = child_id;
                t.name = name;
                t.parent_group_id = Some(track_id);
            }));
            routes[port] = Some(common::model::AuxOutputRoute::to_track(child_id));
        }

        // Insert the new children right after the source track so they appear
        // grouped under it in the arrangement.
        let insert_at = self
            .song_doc.song()
            .track_index_by_id(track_id)
            .map(|i| i + 1)
            .unwrap_or(self.song_doc.song().tracks.len());
        for (k, child) in new_children.into_iter().enumerate() {
            self.edit_song(|song| song.tracks.insert(insert_at + k, child));
        }

        // Wire the source plugin's aux outputs to the (new or kept) children.
        self.edit_song(move |song| {
            if let Some(track) = song.track_by_id_mut(track_id)
                && let Some(inst) = track.devices.get_mut(device_index as usize)
            {
                inst.aux_outputs = routes;
            }
        });

        self.resize_track_peak_display();
    }

    /// `AppEvent::RemoveDevice` の dispatcher。 削除する plugin の最新
    /// state を取ってから Undo snapshot + 削除を行う。
    pub(crate) fn remove_device(&mut self, index: u32) {
        // master 選択時は cursor_track_id == MASTER_TRACK_ID (Vec に居ない)。
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };

        if !self.song_has_plugin() {
            self.remove_device_inner(track_id, index);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::RemoveDevice { track_id, index },
        ));
    }

    /// 単一デバイスチェーン: `Track.devices` / `master_fx_chain` の指定 index の
    /// device を `Vec::remove` する。host への RemoveSlotPlugin + GUI cleanup +
    /// cache 削除 + 後続 index shift + device_index addressing の参照
    /// (automation lane / mod routing / MIDI binding) の追従 + LoadSong flush
    /// を行う。
    pub(crate) fn remove_device_inner(&mut self, track_id: u32, index: u32) {
        // **GUI lifecycle**: close the editor BEFORE removing the
        // plugin. cleanup_slot_gui sends CloseSlotGui so the plugin-host tears
        // the editor window down while the plugin is still at this index —
        // after RemoveSlotPlugin the chain shifts (Vec::remove), so a
        // post-remove close would target a shifted neighbor. RemoveSlotPlugin
        // also closes the editor by stable plugin id as a backstop, and shifts
        // the remaining open-state keys to match the new chain indices.
        self.cleanup_slot_gui(track_id, index);
        // 開いている映像 FX param パネルが同トラックなら閉じる
        // (削除で device index がずれて別 device を指すのを防ぐ)。
        if self.ui_ephemeral.open_video_fx_params.is_some_and(|(t, _)| t == track_id) {
            self.ui_ephemeral.open_video_fx_params = None;
        }
        // 汎用 param パネルも同様に閉じる。
        if self.ui_ephemeral.open_plugin_params.is_some_and(|(t, _)| t == track_id) {
            self.ui_ephemeral.open_plugin_params = None;
        }
        // v29: 安定 device id でアドレスする (song からは chain.remove 前の
        // この時点で引ける)。 video device 等 host に居ないものは host 側が
        // no-op で無視する。
        let removed_device_id = device_id_at(self.song_doc.song(), track_id, index);
        if let Some(device_id) = removed_device_id {
            self.send_plugin(PluginCommand::RemoveSlotPlugin { device_id });
            // load に失敗した device は plugin_host に instance が無く
            // `SlotPluginUnloaded` が返って来ない。 「未ロード」 entry を
            // ここで落とさないと、 消したはずの device がインスペクタの
            // 失敗リストに残り続ける。
            self.ipc.failed_plugin_loads.remove(&device_id);
        }
        // cache から該当 entry を即時削除。 SlotPluginUnloaded event 到着前に
        // reconcile が走っても stale entry を見ないようにする防御策。
        self.ipc.loaded_slots.remove(&(track_id, index));
        // index 以降の loaded_slots / plugin_params を 1 つ前へ詰める (Vec::remove
        // 後の device index と整合させる)。open_plugin_guis は cleanup_slot_gui →
        // shift_slot_gui_keys が既に shift 済み。
        self.shift_device_caches_after_remove(track_id, index);

        // song を書き換え。master は master_fx_chain、 通常 track は devices。
        let Some(Some(removed_id)) = self.edit_song(|song| {
            let chain: Option<&mut Vec<common::model::PluginInstance>> =
                if track_id == common::model::MASTER_TRACK_ID {
                    Some(&mut song.master_fx_chain)
                } else {
                    song.tracks
                        .iter_mut()
                        .find(|t| t.id == track_id)
                        .map(|t| &mut t.devices)
                };
            let chain = chain?;
            let i = index as usize;
            if i >= chain.len() {
                return None;
            }
            let removed = chain.remove(i);
            // VOICEVOX builtin (= vocal track の音源) を外したら vocal 状態も解除
            // (vocal 性は VOICEVOX device の有無に追従)。master には適用しない。
            if track_id != common::model::MASTER_TRACK_ID
                && removed.format == PluginFormat::Builtin
                && removed.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
                && let Some(track) = song.tracks.iter_mut().find(|t| t.id == track_id)
            {
                track.source = InstrumentSource::None;
            }
            // Transform 配置 device を外したら group_transform を消す
            // (device-gate で配置は即無効になるが、残すと ensure_ids が次回ロードで device を
            // 再生成してしまう)。同 track に別の Transform device が残っていれば保持。
            if track_id != common::model::MASTER_TRACK_ID
                && removed.plugin_id == common::video_fx::TRANSFORM_ID
                && let Some(track) = song.tracks.iter_mut().find(|t| t.id == track_id)
                && !track
                    .devices
                    .iter()
                    .any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID)
            {
                track.group_transform = None;
            }
            Some(removed.id)
        }) else {
            return;
        };
        // (review) 削除 device を指す参照 (automation lane / mod routing /
        // MIDI binding) を落とす。 v29: 参照は安定 device_id なので「詰め」は
        // 不要になり、 dangling id の除去だけ行う。
        self.remap_device_refs_after_remove(track_id, removed_id);
        // song 更新を engine へ flush (= schedule から削除 device の dispatch を
        // 落とす)。 これが無いと次の編集まで stale schedule のまま destroyed
        // plugin へ dispatch し続け、 remap した lane も engine へ届かない。
    }

    /// device を `Vec::remove` した後、 削除 device (安定 id =
    /// `removed_device_id`) を指す automation lane / mod routing /
    /// MIDI binding を丸ごと削除する。 v29 で参照が id 化されたので、 旧
    /// positional 版が行っていた「後続 index の詰め」 は不要になった。
    /// point / clip 選択は stable な lane_id 参照なので、 lane 削除後は
    /// dangling 解決 (= None) で無害に落ちる。
    pub(crate) fn remap_device_refs_after_remove(&mut self, track_id: u32, removed_device_id: u64) {
        if removed_device_id == 0 {
            return; // 未採番 device (来ないはず) — 誤って全 lane を消さない
        }
        // 戻り値 = 残すか。
        fn keeps(
            target: &common::model::AutomationTarget,
            removed_device_id: u64,
        ) -> bool {
            !matches!(
                target,
                common::model::AutomationTarget::PluginParam { device_id, .. }
                    if *device_id == removed_device_id
            )
        }
        self.edit_song(move |song| {
            if track_id == common::model::MASTER_TRACK_ID {
                song.song_lanes
                    .retain(|l| keeps(&l.target, removed_device_id));
                song.song_mod_routings
                    .retain(|r| keeps(&r.target, removed_device_id));
            } else if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) {
                t.automation_lanes
                    .retain(|l| keeps(&l.target, removed_device_id));
                t.mod_routings
                    .retain(|r| keeps(&r.target, removed_device_id));
            }
            song.midi_bindings.retain(|b| {
                !matches!(
                    &b.target,
                    common::model::BindingTarget::PluginParam { device_id, .. }
                        if *device_id == removed_device_id
                )
            });
        });
    }

    /// device を index で `Vec::remove` した後、`loaded_slots` / `plugin_params`
    /// の `(track, idx)` キーのうち `idx > index` のものを 1 つ前へ詰める。
    /// open_plugin_guis は `shift_slot_gui_keys` が別途扱う。
    pub(crate) fn shift_device_caches_after_remove(&mut self, track_id: u32, index: u32) {
        // loaded_slots
        let mut moves: Vec<(u32, LoadedSlotInfo)> = Vec::new();
        self.ipc.loaded_slots.retain(|&(t, idx), v| {
            if t == track_id && idx > index {
                moves.push((idx - 1, v.clone()));
                false
            } else {
                true
            }
        });
        for (idx, v) in moves {
            self.ipc.loaded_slots.insert((track_id, idx), v);
        }
        // plugin_params
        let mut pmoves: Vec<(u32, Vec<common::protocol::PluginParamInfo>)> = Vec::new();
        self.ipc.plugin_params.retain(|&(t, idx), v| {
            if t == track_id && idx > index {
                pmoves.push((idx - 1, v.clone()));
                false
            } else {
                true
            }
        });
        for (idx, v) in pmoves {
            self.ipc.plugin_params.insert((track_id, idx), v);
        }
        // slot_has_gui: plugin_params と同じ index シフト。
        let mut gmoves: Vec<(u32, bool)> = Vec::new();
        self.ipc.slot_has_gui.retain(|&(t, idx), v| {
            if t == track_id && idx > index {
                gmoves.push((idx - 1, *v));
                false
            } else {
                true
            }
        });
        for (idx, v) in gmoves {
            self.ipc.slot_has_gui.insert((track_id, idx), v);
        }
    }

    /// `(track_id, device_index)` のプラグイン GUI を閉じ、 同 track の後続
    /// device (= `idx > index`) の open-state key を 1 つずつ前にずらす
    /// (`Vec::remove` 後の chain index と整合させるため)。 実 window
    /// は plugin-host プロセス所有なので、 破棄は `CloseSlotGui` を送って B 側に
    /// 行わせる。 RemoveSlotPlugin / RemoveTrack も B 側で window を破棄するので
    /// 二重でも idempotent。
    #[cfg(windows)]
    pub(crate) fn cleanup_slot_gui(&mut self, track_id: u32, index: u32) {
        // open 中なら B に閉じてもらう (= DestroyWindow は plugin-host 側)。
        // v29: device_id addressing。 host 実体の SSoT である loaded_slots を
        // 優先し (reconcile の「song に無い device」 でも解決できる)、
        // 無ければ song から引く。
        if self.ipc.open_plugin_guis.remove(&(track_id, index)) {
            let device_id = self
                .ipc.loaded_slots
                .get(&(track_id, index))
                .map(|info| info.device_id)
                .or_else(|| device_id_at(self.song_doc.song(), track_id, index));
            if let Some(device_id) = device_id {
                self.send_plugin(PluginCommand::CloseSlotGui { device_id });
            }
        }
        self.shift_slot_gui_keys(track_id, index);
    }

    #[cfg(not(windows))]
    pub(crate) fn cleanup_slot_gui(&mut self, _track_id: u32, _index: u32) {}

    /// 単一デバイスチェーン: `removed_idx` を `Vec::remove` した後、
    /// `idx > removed_idx` な open-state key を 1 つずつ前にずらす。
    #[cfg(windows)]
    pub(crate) fn shift_slot_gui_keys(&mut self, track_id: u32, removed_idx: u32) {
        let mut moves: Vec<u32> = self
            .ipc.open_plugin_guis
            .iter()
            .filter(|&&(t, idx)| t == track_id && idx > removed_idx)
            .map(|&(_, idx)| idx)
            .collect();
        // 低 index 側を先に詰める (collision-free)。
        moves.sort_unstable();
        for idx in moves {
            if self.ipc.open_plugin_guis.remove(&(track_id, idx)) {
                self.ipc.open_plugin_guis.insert((track_id, idx - 1));
            }
        }
    }

    /// `plugin_host` から `AllPluginStates` 受信。 全 plugin の最新
    /// state を Song に書き戻したあと、 [`AppData::pending_state_queue`]
    /// の front を取り出して完了処理 (save または deferred edit) を実行する。
    /// queue に後続がある場合は次の `RequestAllStates` を改めて発行し、
    /// 連続 deferred edit が個別に最新 state を捕まえられるようにする。
    pub(crate) fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        // in-flight だった round-trip の応答が来た。 watchdog の deadline を
        // 解除する。 この後 queue に後続があれば dispatch_front_state_request が再武装する。
        self.ipc.state_request_sent_at = None;
        // live song の plugin state を最新化する (= dirty 判定の整合と、
        // Deferred の Undo snapshot が最新 knob を捕まえるため)。 queue が空
        // だった場合 (= 想定外タイミングの応答) でも害はない。 Save の serialize
        // 対象は live ではなく凍結 snapshot なので、 下の match 内で snapshot 側に
        // も別途適用する。
        self.song_doc
            .write_back_plugin_state(|song| Self::apply_plugin_states_to(song, &states));
        // Phase 6 review (silent corruption fix): plugin_host 側で
        // `state_save()` が `Err` を返したエントリは `SlotState.error`
        // 経由で報告される。 旧コードはこれを `.ok().flatten()` で握り
        // つぶしていて、 保存 file に空 state を書き → 次回開いたとき
        // plugin が default 状態に戻る silent corruption になっていた。
        // 集計して status_message に表示し、 ユーザーが「N 個の plugin
        // state が保存されなかった」 と認識できるようにする。 件数が多い
        // と message が長くなりすぎるので、 先頭 3 件のみ詳細を出し
        // 残りは件数集約。
        let failed: Vec<&SlotState> = states
            .iter()
            .filter(|s| s.error.is_some())
            .collect();
        if !failed.is_empty() {
            let mut msg = format!("Plugin state 保存失敗 ({} 件): ", failed.len());
            for (i, s) in failed.iter().take(3).enumerate() {
                if i > 0 {
                    msg.push_str(", ");
                }
                // v29: device_id keyed。 ユーザー向けには (track, device) の
                // 見える座標に逆引きして表示する (解決不能なら id を出す)。
                match find_device_by_id(self.song_doc.song(), s.device_id) {
                    Some((track, index)) => {
                        msg.push_str(&format!("track {track} device {index}"));
                    }
                    None => {
                        msg.push_str(&format!("device {}", s.device_id));
                    }
                }
            }
            if failed.len() > 3 {
                msg.push_str(&format!(" ... +{}", failed.len() - 3));
            }
            tracing::error!(failed_count = failed.len(), %msg, "plugin state save failures");
            self.ui_ephemeral.status_message = msg;
        }
        let Some(req) = self.ipc.pending_state_queue.pop_front() else {
            return;
        };
        match req {
            PendingStateRequest::Save {
                path,
                snapshot,
                snap_epoch,
            } => {
                // snapshot は dispatch_front_state_request が **この save の
                // RequestAllStates を送る瞬間** に充填しているはず。 受け取った
                // states (= その RequestAllStates の応答) はその瞬間の host layout を
                // 反映するので、 snapshot のスロット配置と一致し、 位置 index 適用でも
                // 誤適用が起きない。 万一 None (想定外) なら防御的に live を凍結する。
                let snap_epoch = if snapshot.is_some() {
                    snap_epoch
                } else {
                    self.song_doc.edit_epoch()
                };
                let mut snapshot =
                    snapshot.unwrap_or_else(|| Box::new(self.song_doc.song().clone()));
                Self::apply_plugin_states_to(&mut snapshot, &states);
                self.finish_save(snapshot, path, snap_epoch);
            }
            PendingStateRequest::Deferred(edit) => {
                // ここで初めて Undo snapshot を push する。 Song に
                // 最新 state が入った状態を捕まえるため (plugin が
                // 削除される編集を Undo すると knob 値が復元される)。
                self.execute_deferred_edit(edit);
            }
            PendingStateRequest::CopyToClipboard { track_ids } => {
                // copy は Song 不変なので undo を積まない。最新 state
                // 込みで serialize して pending_clipboard_write に積むだけ。
                self.copy_tracks_inner(&track_ids);
            }
        }
        // 後続の request が積まれていれば、 改めて `RequestAllStates` を発行して
        // 次の応答待ちに入る。 ここで「直前の edit が走ったあとの最新 state」 を
        // 再取得することで、 各 deferred edit が自前の knob snapshot を持つ。 さらに
        // 新たな front が Save なら、 dispatch_front_state_request が **この瞬間**
        // (= 先行 Deferred が live layout を確定させた直後) に live を凍結するので、
        // その Save の snapshot は返ってくる state と同じ layout になる。
        if !self.ipc.pending_state_queue.is_empty() {
            self.dispatch_front_state_request();
        } else if let Some(action) = self.ui_ephemeral.guard_pending_action.take() {
            // round-trip が全て drain した。 in-flight 中に保留していた
            // ガード操作 (New/Open/Open Recent/終了) を、 deferred edit / save 反映後の
            // **最新 dirty 状態で再評価** する (= clean なら実行、 dirty なら確認モーダル)。
            // dirty は edit_epoch 由来の O(1) 派生なので明示的な recompute は不要。
            // queue は空なので破壊操作も安全に走る。
            self.request_guarded_action(action);
        }
        // 「保存して終了」 の完了判定は `finish_save` (save 成否が分かる場所) が行う。
    }

    /// `AllPluginStates` 受信後に呼ばれる。 deferred edit を実際に実行
    /// する。 inner 関数群は `push_undo_snapshot` を呼ばない (= 上の
    /// `on_all_states_from_child` 側で push 済みであり、 二重 push を
    /// 避けるため)。
    pub(crate) fn execute_deferred_edit(&mut self, edit: DeferredEdit) {
        match edit {
            DeferredEdit::DeleteTracks { track_ids } => self.delete_tracks_inner(&track_ids),
            DeferredEdit::UngroupTracks { track_ids } => {
                self.action_ungroup_tracks_inner(&track_ids)
            }
            DeferredEdit::RemoveDevice { track_id, index } => {
                self.remove_device_inner(track_id, index)
            }
            DeferredEdit::CutTracks { track_ids } => self.cut_tracks_inner(&track_ids),
            DeferredEdit::DuplicateTracks { track_ids, linked } => {
                self.duplicate_tracks_inner(&track_ids, linked)
            }
        }
    }

}
