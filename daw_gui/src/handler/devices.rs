//! handler::devices — plugin load/gui/chain/sidechain/parallel-out + device 削除 + state round-trip
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::InstrumentSource;
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, PlatformWindowHandle, PluginCommand, SlotState};

impl AppData {
    // -------- Plugin GUI bridge --------------------------------------------

    /// r.md #65: エディタ窓のジオメトリが確定した (open 直後 / ユーザーのドラッグ
    /// 終了 / プラグイン起点リサイズ完了 / close 直前)。
    ///
    /// 窓を所有するのは plugin-host なので daw_gui は **記録するだけ**。値は
    /// `snapshot_view_state` でプロジェクトへ書き出され、次の `OpenSlotGuiEmbedded`
    /// に載って復元される。「見方の都合」なので dirty は立てない
    /// (memory `project_dirty_flag_rule`)。
    pub(crate) fn on_gui_geometry(
        &mut self,
        device_id: u64,
        geometry: common::model::EditorWindowGeometry,
    ) {
        // 縮退サイズは記録しない (plugin-host 側の `persistable_geometry` が既に
        // 弾いているが、境界でも塞ぐ — 0 を保存すると次回 open で 1×1 の窓になる)。
        if geometry.width == 0 || geometry.height == 0 {
            return;
        }
        self.ui_prefs.plugin_editor_windows.insert(device_id, geometry);
    }

    pub(crate) fn on_gui_closed(&mut self, device_id: u64) {
        // The plugin-host process tore the editor window down (user clicked
        // the window's ✕, or the plugin self-closed). Drop our open-state.
        self.ipc.open_plugin_guis.remove(&device_id);
        // r.md #65: エディタの open / close は **2 プロセスに跨って往復する**ので、
        // 片側のログだけでは「誰が閉じて誰が開き直したか」が決まらない。
        // 頻度は人の操作と同じなので info で常設する。
        tracing::info!(
            device_id,
            still_open = self.ipc.open_plugin_guis.len(),
            "plugin editor closed (SlotGuiClosed from plugin-host)"
        );
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
        // chain の該当位置に `PluginInstance` を書き戻すために、 いまの
        // 所属 track と位置を **その場で引き直す** (保持はしない)。
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
        // device 単位での load 状態 cache。 reconcile の device-level diff
        // (Undo で plugin 構成が変化した場合の同期) と、 track 削除前の
        // `ClosePluginShmem` の対象確認 (use-after-free deadlock 防止) に使う。
        self.ipc.loaded_devices.insert(
            device_id,
            LoadedDeviceInfo {
                plugin_id_str: id.clone(),
            },
        );
        // 注意: ここで `ensure_first_track()` を呼んではいけない。 子プロセスの
        // 応答が Song の構造 (トラック) を作ると、 track を 1 本も持たず master fx
        // だけを持つプロジェクトを開いたときに、 その load 応答が幽霊トラック
        // "Track 1" を生やして **開いただけで `*`** が付く (r.md #9)。
        // 空プロジェクトへの最初の 1 本はユーザー操作の側 (plugin picker) が作る。

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
                    // r.md #9: port 解決の規則は `PortConfig::resolve` に一本化して
                    // ある。 ここで DB を優先すると、 DB と保存値が食い違う環境で
                    // load 応答のたびに instance が書き換わり「開いただけで `*`」。
                    ..common::model::PluginInstance::with_ports(
                        id,
                        format,
                        common::port_config::PortConfig::resolve(existing_ports, db_ports),
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
        if let Some(open_gui) = self.ipc.pending_added_plugin_finalize.remove(&device_id)
            && open_gui
        {
            self.ipc.gui_open_requests.push(device_id);
        }

        // A7: this load is done. If Play was queued waiting for the
        // last plugin to register on the audio side, fire it now.
        self.ipc.pending_plugin_loads.remove(&device_id);
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.ui_ephemeral.status_message.clear();
            self.fire_pending_play();
        } else if !self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.ui_ephemeral.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.ipc.pending_plugin_loads.len()
            );
        }

        // PR-V3: builtin VOICEVOX が load されたら、 直後に歌詞 metadata を
        // flush して背景 synth を trigger する。 device が `loaded_devices`
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
        self.ipc.pending_added_plugin_finalize.remove(&device_id);
        // pending_play 解放: A7 と同じロジック (`on_plugin_loaded_from_child`
        // と対称)。 失敗で空になったタイミングで queue Play を flush する。
        if self.ipc.pending_plugin_loads.is_empty() && self.transport.pending_play {
            self.ui_ephemeral.status_message =
                format!("プラグイン読み込み失敗: {plugin_id} ({reason})");
            self.fire_pending_play();
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
    pub(crate) fn reload_device(&mut self, device_id: u64) {
        let song = self.song_doc.song();
        let Some(inst) = find_device_by_id(song, device_id)
            .and_then(|(track_id, index)| device_at(song, track_id, index))
            .cloned()
        else {
            return;
        };
        // 内蔵映像 FX は plugin_host に載らない device なので再 load の
        // 対象にならない (そもそも load 失敗も起きない)。
        if inst.ports.is_video() {
            return;
        }
        let name = self.resolve_name(&inst.plugin_id);
        if self.send_set_slot_plugin(
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
    /// device 単位の daw_gui ローカル状態をクリーンアップする。
    /// shmem の close は必ず先行する `SlotPluginShmemReleased` が担うので
    /// ここでは送らない (SSoT — 二重送信しない)。
    pub(crate) fn on_plugin_unloaded_from_child(&mut self, device_id: u64) {
        // device が空になったので「未ロード」表示も畳む (再 load の対象は
        // song に残っている device だけ)。
        self.ipc.failed_plugin_loads.remove(&device_id);
        self.forget_device_caches(device_id);
        // builtin VOICEVOX が外れたら合成状態 entry も消す (busy のまま残ると
        // overlay / スピナーが消えない)。plugin host の deactivate も idle を報告するが二重防御。
        self.voicevox.voicevox_synth_status.remove(&device_id);
        // r.md #27: metadata 差分キャッシュも live device に揃える (unload された
        // device の stale entry を残さない。voicevox_synth_status と対称)。
        self.voicevox.voicevox_metadata_sent.remove(&device_id);
        // 消えた device の PDC 寄与も畳む (0 = entry を落とす)。
        self.set_device_latency(device_id, 0);
    }

    /// r.md #71 (プラグインのコピー / 移動): device 単位の帳簿から 1 台分を落とす。
    ///
    /// device_id keyed になったので **消える瞬間に明示的に消す**のが正しい
    /// (positional 時代は「index を詰めれば拾える」という理由で放置していたが、
    /// id keyed では詰める操作が無いので放置 = 永久に残る)。 落とす経路は
    /// unload 通知 / device 削除 / reconcile の RemoveDevice の 3 つ。
    pub(crate) fn forget_device_caches(&mut self, device_id: u64) {
        self.ipc.loaded_devices.remove(&device_id);
        self.ipc.plugin_params.remove(&device_id);
        self.ipc.slot_has_gui.remove(&device_id);
        self.ipc
            .plugin_param_values
            .retain(|k, _| k.device_id != device_id);
    }

    /// plugin host が報告した device の processing latency を engine へ中継する。
    ///
    /// r.md #9: 報告値は **実行時の観測値であって曲の中身ではない** ので `Song` に
    /// 書かない。 旧実装は track ごとに合計して `Track::reported_latency_samples`
    /// へ `edit_song_checked` で書き戻していたため、
    ///
    /// - 保存ファイルに合計値が載る (プラグイン更新 / サンプルレート違いで乖離する)
    /// - device の load 応答は 1 台ずつ届くので、 途中経過の**部分合計**や
    ///   「まだ応答が来ていない track = plugin 無し」 という誤認で 0 に潰す書き込みが
    ///   走り、 **保存済みプロジェクトを開いただけで `*` が付いた**
    ///
    /// という 2 つの問題があった。 いまは device 単位のまま engine へ送り、
    /// track / master の合計は `compile_schedule` が chain から導出する
    /// (集計の実装を GUI と engine に二重に持たない)。
    pub(crate) fn on_plugin_latency_changed(&mut self, device_id: u64, samples: u32) {
        self.set_device_latency(device_id, samples);
    }

    /// `AudioCommand::SetDeviceLatency` を送る唯一の口。
    fn set_device_latency(&mut self, device_id: u64, samples: u32) {
        self.send_audio(AudioCommand::SetDeviceLatency { device_id, samples });
    }

    pub(crate) fn toggle_slot_gui(&mut self, device_id: u64) {
        // r.md #71 (プラグインのコピー / 移動): アドレスは安定 device_id 一本。
        // cursor track に依存しないので、 表示チェーンが切り替わっても
        // 「どの device のボタンを押したか」 が変わらない。
        let song = self.song_doc.song();
        let device = find_device_by_id(song, device_id)
            .and_then(|(track_id, index)| device_at(song, track_id, index));
        // 映像 FX (色補正 / Transform 等) は専用の video_fx パネル。 ただし字幕
        // (`builtin.video.subtitle`) は video device だが video_fx def を持たず、
        // 専用パラメータは Text Event セクション (= Par パネルで描画) なので、 ここで
        // 弾いて下の open_plugin_params 経路へ流す。
        if let Some(d) = device
            && d.ports.is_video()
            && d.plugin_id != common::plugin_db::SUBTITLE_ID
        {
            self.ui_ephemeral.open_plugin_params = None; // 2 種のインライン param パネルは相互排他。
            self.ui_ephemeral.open_video_fx_params =
                if self.ui_ephemeral.open_video_fx_params == Some(device_id) {
                    None
                } else {
                    Some(device_id)
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
                .get(&device_id)
                .copied()
                .unwrap_or(true);
        if !has_embedded_gui {
            self.ui_ephemeral.open_video_fx_params = None; // 2 種のインライン param パネルは相互排他。
            self.ui_ephemeral.open_plugin_params =
                if self.ui_ephemeral.open_plugin_params == Some(device_id) {
                    None
                } else {
                    Some(device_id)
                };
            return;
        }
        // 既に開いていれば閉じる (toggle)。開いていなければ open_slot_gui で開く。
        // open 状態は open_plugin_guis (id set) で追跡。実 window は
        // plugin-host プロセスが所有するので、close は CloseSlotGui を送って
        // B 側に破棄させ、SlotGuiClosed の受信で set から除去する。
        let is_open = self.ipc.open_plugin_guis.contains(&device_id);
        // r.md #65: 「GUI ボタンが押された」を 1 行で残す。押すたびに open / close が
        // 交互になるので、**このログが 2 行連続で出れば人が 2 回押した**と確定する
        // (= 自動で開き直っているのではない)。
        tracing::info!(device_id, is_open, "toggle_slot_gui (GUI button)");
        if is_open {
            self.send_plugin(PluginCommand::CloseSlotGui { device_id });
            return;
        }
        self.open_slot_gui(device_id);
    }

    /// 指定 device のプラグイン GUI を embedded container window で開く。
    /// 既に開いていれば何もしない (重複 open 防止)。Windows 専用
    /// (他 OS では no-op)。`toggle_slot_gui` (手動トグル) と plugin 追加時の自動
    /// open の両方から使う。
    #[track_caller]
    pub(crate) fn open_slot_gui(&mut self, device_id: u64) {
        // r.md #65: **呼び出し元を値の中に入れる。** 「開き直った」を見たとき、
        // 呼んだのが `toggle_slot_gui` (= ユーザーが GUI ボタンを押した) なのか
        // `drain_pending_gui_opens` (= plugin load 完了の自動 open) なのかで
        // 原因がまったく別になるのに、ログからは区別できなかった。
        tracing::info!(
            device_id,
            already_open = self.ipc.open_plugin_guis.contains(&device_id),
            caller = %std::panic::Location::caller(),
            "open_slot_gui"
        );
        #[cfg(windows)]
        {
            if self.ipc.open_plugin_guis.contains(&device_id) {
                return;
            }
            let label = self.device_display_name(device_id);
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
            // r.md #65: エディタコンテナ窓の owner にする **本体窓** (preview 窓ではない)。
            // `main_window_hwnd` は runner が本体窓の生成時に 1 度だけ書き込む値で、
            // preview 窓はここに載らない。0 は「窓が無い」なので `from_raw` が落とす。
            let owner_main_window = self
                .ui_ephemeral
                .main_window_hwnd
                .and_then(|hwnd| PlatformWindowHandle::from_raw(hwnd as u64));
            if owner_main_window.is_none() {
                // 起きるのは本体窓の生成前だけ (frame loop 由来の open では起きない)。
                // plugin-host 側は owner 無し + TOOLWINDOW 無しで開くので、
                // 「Alt+Tab から消えたのに潜る」状態にはならない。
                tracing::warn!(
                    device_id,
                    "opening plugin editor without an owner window (main window not ready)"
                );
            }
            self.ipc.open_plugin_guis.insert(device_id);
            self.send_plugin(PluginCommand::OpenSlotGuiEmbedded {
                device_id,
                title: format!("Plugin — {label}"),
                // r.md #65: 前回このプロジェクトで閉じたときの窓の位置 / サイズ。
                // 位置は常に、サイズは plugin が resizable のときだけ plugin-host が使う。
                geometry: self.ui_prefs.plugin_editor_windows.get(&device_id).copied(),
                owner_main_window,
            });
            // r.md #36: 「キーを全部プラグインに送る」 の現在値を open のたびに同期する
            // (plugin-host は再起動で状態を失う / device_id は open まで意味を持たない)。
            let song = self.song_doc.song();
            let send_all = find_device_by_id(song, device_id)
                .and_then(|(t, i)| device_at(song, t, i))
                .is_some_and(|p| p.send_all_keys_to_plugin);
            self.send_plugin(PluginCommand::SetEditorSendAllKeys {
                device_id,
                enabled: send_all,
            });
        }
        #[cfg(not(windows))]
        {
            let _ = device_id;
        }
    }

    /// エディタ窓のタイトルに出す表示名 (`"Master / Comp"` / `"Bass / Serum"`)。
    /// 所属 track は `find_device_by_id` で毎回引き直す (r.md #71 プラグインの
    /// コピー / 移動: device は別トラックへ移動しうるので保持しない)。
    #[cfg(windows)]
    fn device_display_name(&self, device_id: u64) -> String {
        let song = self.song_doc.song();
        let Some((track_id, index)) = find_device_by_id(song, device_id) else {
            return "(unknown)".into();
        };
        let Some(name) = device_at(song, track_id, index).map(|p| self.resolve_name(&p.plugin_id))
        else {
            return "(unknown)".into();
        };
        if track_id == common::model::MASTER_TRACK_ID {
            format!("Master / {name}")
        } else {
            match song.tracks.iter().find(|t| t.id == track_id) {
                Some(t) => format!("{} / {}", t.name, name),
                None => name,
            }
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
        for device_id in std::mem::take(&mut self.ipc.gui_open_requests) {
            self.open_slot_gui(device_id);
        }
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

        // r.md #71 (プラグインのコピー / 移動): 「ロード中は並べ替えできない」
        // という旧制約は **撤去した**。 理由 (positional cache の再キーがずれる)
        // が消えたため — 帳簿はすべて安定 device_id keyed で、 並べ替えても
        // キーが動かない。 プロセス間も device_id addressing なので、
        // 続く `LoadSong` (epoch flush) が処理順を Song から再 compile するだけ。

        // 新順での device 列を組む (new[i] = old[order[i]])。
        let new_devices: Vec<common::model::PluginInstance> =
            order.iter().map(|&o| old_devices[o].clone()).collect();

        // song を書き換え。
        if is_master {
            self.edit_song(move |song| song.master_fx_chain = new_devices);
        } else {
            self.edit_song_checked(move |song| {
                if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) {
                    t.devices = new_devices;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// PR4 sidechain: route a track's output into a plugin's `aux_in_port`.
    /// `source = None` disconnects. The plugin's
    /// `PluginInstance.aux_inputs[port]` slot is created on demand;
    /// shorter vectors are extended with `None` placeholders so port `port`
    /// becomes addressable. After mutation we re-`flush_song_sync`
    /// so `compile_schedule` regenerates the `SidechainTap` ops.
    /// r.md #36: この device のエディタ窓で 「キーを全部プラグインに送る」 かを設定する。
    /// project に保存し (undo 対象)、 plugin-host にも即時反映する。
    pub(crate) fn set_plugin_send_all_keys(&mut self, device_id: u64, enabled: bool) {
        self.edit_song_checked(|song| {
            let Some(inst) = device_mut_by_id(song, device_id) else {
                return false;
            };
            if inst.send_all_keys_to_plugin == enabled {
                return false;
            }
            inst.send_all_keys_to_plugin = enabled;
            true
        });
        self.send_plugin(PluginCommand::SetEditorSendAllKeys { device_id, enabled });
    }

    pub(crate) fn set_sidechain_source(&mut self, device_id: u64, port: u8, source: Option<u32>) {
        self.edit_song_checked(|song| {
            let Some(inst) = device_mut_by_id(song, device_id) else {
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

    /// パラアウト (docs/plan_paraout.md): route one aux output `port` of device
    /// `device_id` to `dest` (or `None` = unrouted).
    /// Mirror of `set_sidechain_source` (aux_outputs instead of aux_inputs).
    /// Used by the inspector dropdown for re-adjustment; not auto-undoable
    /// (matches sidechain), but marks dirty + recompiles via
    /// `flush_song_sync`.
    pub(crate) fn set_parallel_output_route(
        &mut self,
        device_id: u64,
        port: u8,
        dest: Option<u32>,
    ) {
        self.edit_song_checked(|song| {
            let Some(inst) = device_mut_by_id(song, device_id) else {
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
    pub(crate) fn explode_parallel_out(&mut self, device_id: u64) {
        let Some((track_id, device_index)) =
            find_device_by_id(self.song_doc.song(), device_id)
        else {
            return;
        };
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

    /// `AppEvent::RemoveDevices` の dispatcher。 削除する plugin の最新
    /// state を取ってから Undo snapshot + 削除を行う。
    ///
    /// r.md #71 (プラグインのコピー / 移動): 複数選択を **1 件にまとめて** 積む
    /// (id ごとに enqueue すると round-trip が分かれて undo が N ステップに割れる)。
    pub(crate) fn remove_devices(&mut self, device_ids: Vec<u64>) {
        if device_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() {
            self.remove_devices_inner(&device_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::RemoveDevices {
            device_ids,
        }));
    }

    /// 単一デバイスチェーン: 指定 device を所属チェーンから `Vec::remove` する。
    /// host への RemoveSlotPlugin + GUI cleanup + cache 削除 + device_id
    /// addressing の参照 (automation lane / mod routing / MIDI binding) の
    /// dangling 除去を行う。 **全 device を 1 回の `edit_song` で消す** ので
    /// undo は 1 ステップ。
    pub(crate) fn remove_devices_inner(&mut self, device_ids: &[u64]) {
        // 実在する device だけに絞る (stale id は黙って捨てる = 削除済み device
        // への stale event は正常系)。 所属 track も先に控えておく — chain から
        // 外した後では引けない。
        let targets: Vec<(u64, u32)> = device_ids
            .iter()
            .filter_map(|&id| {
                find_device_by_id(self.song_doc.song(), id).map(|(track_id, _)| (id, track_id))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        for &(device_id, _) in &targets {
            // **GUI lifecycle**: close the editor BEFORE removing the plugin.
            // cleanup_slot_gui sends CloseSlotGui so the plugin-host tears the
            // editor window down. RemoveSlotPlugin also closes the editor by
            // stable device id as a backstop (idempotent)。
            self.cleanup_slot_gui(device_id);
            // 開いているインライン param パネルが **消す device を指していたら**
            // 閉じる (別 device を指しているなら触らない — id keyed なので
            // 「同トラックだから」で巻き込む必要が無い)。
            if self.ui_ephemeral.open_video_fx_params == Some(device_id) {
                self.ui_ephemeral.open_video_fx_params = None;
            }
            if self.ui_ephemeral.open_plugin_params == Some(device_id) {
                self.ui_ephemeral.open_plugin_params = None;
            }
            // video device 等 host に居ないものは host 側が no-op で無視する。
            self.send_plugin(PluginCommand::RemoveSlotPlugin { device_id });
            // load に失敗した device は plugin_host に instance が無く
            // `SlotPluginUnloaded` が返って来ない。 「未ロード」 entry を
            // ここで落とさないと、 消したはずの device がインスペクタの
            // 失敗リストに残り続ける。
            self.ipc.failed_plugin_loads.remove(&device_id);
            // cache から該当 entry を即時削除。 SlotPluginUnloaded event 到着前に
            // reconcile が走っても stale entry を見ないようにする防御策。
            self.forget_device_caches(device_id);
        }

        // song を書き換え。 全 device を 1 回の edit_song で消す (undo 1 step)。
        let ids: Vec<u64> = targets.iter().map(|&(id, _)| id).collect();
        let removed = self.edit_song(move |song| {
            let mut removed: Vec<(u32, common::model::PluginInstance)> = Vec::new();
            for &device_id in &ids {
                let Some((track_id, index)) = find_device_by_id(song, device_id) else {
                    continue;
                };
                let Some(chain) = song.fx_chain_by_track_id_mut(track_id) else {
                    continue;
                };
                removed.push((track_id, chain.remove(index as usize)));
            }
            // 副作用は **全部消してから** 評価する。 「2 本ある VOICEVOX の
            // 1 本だけ消す」 が成立するので、 途中の中間状態で判定すると
            // 残っている方まで巻き込む。
            for (track_id, inst) in &removed {
                if *track_id == common::model::MASTER_TRACK_ID {
                    continue; // master は Track ではないので副作用を持たない。
                }
                let Some(track) = song.tracks.iter_mut().find(|t| t.id == *track_id) else {
                    continue;
                };
                // VOICEVOX builtin (= vocal track の音源) を外したら vocal 状態も解除
                // (vocal 性は VOICEVOX device の有無に追従)。 **他に VOICEVOX が
                // 残っていれば保持** — Transform 側と同じ規則にして 1 本にする
                // (r.md #71: 複数選択削除で「2 本のうち 1 本だけ」が起きるようになった)。
                if inst.format == PluginFormat::Builtin
                    && inst.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
                    && !track
                        .devices
                        .iter()
                        .any(|d| d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX)
                {
                    track.source = InstrumentSource::None;
                }
                // Transform 配置 device を外したら group_transform を消す
                // (device-gate で配置は即無効になるが、残すと ensure_ids が次回ロードで device を
                // 再生成してしまう)。同 track に別の Transform device が残っていれば保持。
                if inst.plugin_id == common::video_fx::TRANSFORM_ID
                    && !track
                        .devices
                        .iter()
                        .any(|d| d.plugin_id == common::video_fx::TRANSFORM_ID)
                {
                    track.group_transform = None;
                }
            }
            removed
        });
        let Some(removed) = removed else {
            return;
        };
        // (review) 削除 device を指す参照 (automation lane / mod routing /
        // MIDI binding) を落とす。 v29: 参照は安定 device_id なので「詰め」は
        // 不要になり、 dangling id の除去だけ行う。
        for (track_id, inst) in &removed {
            self.remap_device_refs_after_remove(*track_id, inst.id);
        }
        // 選択集合からも消えた id を落とし、 空になったら last-wins タグを降ろす
        // (正しさは `live_device_ids()` の正規化が担保する。 これは後始末)。
        self.prune_device_selection();
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

    /// この device のプラグイン GUI が開いていれば閉じる。 実 window は
    /// plugin-host プロセス所有なので、 破棄は `CloseSlotGui` を送って B 側に
    /// 行わせる。 `RemoveSlotPlugin` も B 側で window を破棄するので二重でも
    /// idempotent。
    ///
    /// r.md #71 (プラグインのコピー / 移動): open-state は安定 device_id keyed に
    /// なったので、 削除に伴う **key の詰め直しは無い** (旧
    /// `shift_slot_gui_keys` は不変条件 1 が禁じる貼り替え補償コードだった)。
    #[cfg(windows)]
    pub(crate) fn cleanup_slot_gui(&mut self, device_id: u64) {
        if self.ipc.open_plugin_guis.remove(&device_id) {
            self.send_plugin(PluginCommand::CloseSlotGui { device_id });
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn cleanup_slot_gui(&mut self, _device_id: u64) {}

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
            PendingStateRequest::CopyToClipboard(req) => {
                // copy は Song 不変なので undo を積まない。最新 state
                // 込みで serialize して pending_clipboard_write に積むだけ。
                match req {
                    ClipboardCopyRequest::Tracks(track_ids) => {
                        self.copy_tracks_inner(&track_ids);
                    }
                    ClipboardCopyRequest::Devices(device_ids) => {
                        self.copy_devices_inner(&device_ids);
                    }
                }
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
            DeferredEdit::RemoveDevices { device_ids } => {
                self.remove_devices_inner(&device_ids)
            }
            DeferredEdit::RelocateDevices(req) => self.relocate_devices_inner(&req),
            DeferredEdit::CutDevices { device_ids } => self.cut_devices_inner(&device_ids),
            DeferredEdit::CutTracks { track_ids } => self.cut_tracks_inner(&track_ids),
            DeferredEdit::DuplicateTracks { track_ids, linked } => {
                self.duplicate_tracks_inner(&track_ids, linked)
            }
        }
    }

}
