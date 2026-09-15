//! handler::sync — host sync (pull 型 LoadSong) + ARA document/region 同期 + port 解決
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use std::path::{Path, PathBuf};
use common::plugin_db::PluginDatabase;
use common::protocol::{AudioCommand, PluginCommand};

impl AppData {
    // -------- IPC -----------------------------------------------------------

    pub(crate) fn send_audio(&self, msg: AudioCommand) {
        // r.md #16: 全文 Debug dump を info で吐くとログが肥大する (特に LoadSong は
        // 全 song を 1 行に serialize し、 ドラッグ中は毎 frame 送られる)。 debug へ
        // 降格し、 LoadSong は track/clip 数の要約だけにする (payload は落とす)。
        match &msg {
            AudioCommand::LoadSong { song, .. } => {
                let clips: usize = song.tracks.iter().map(|t| t.clips.len()).sum();
                tracing::debug!(tracks = song.tracks.len(), clips, "sending LoadSong to audio");
            }
            other => tracing::debug!(msg = ?other, "sending to audio"),
        }
        let Some(tx) = self.ipc.audio_tx.as_ref() else {
            tracing::warn!("audio sender is not configured");
            return;
        };
        if let Err(e) = tx.send(msg) {
            tracing::error!(error = %e, "failed to enqueue audio command");
        }
    }

    pub(crate) fn send_plugin(&self, msg: PluginCommand) {
        // r.md #16: 全文 Debug dump は debug 降格 (info の既定ログには載せない)。
        tracing::debug!(msg = ?msg, "sending to plugin_host");
        let Some(tx) = self.ipc.plugin_tx.as_ref() else {
            tracing::warn!("plugin sender is not configured");
            return;
        };
        if let Err(e) = tx.send(msg) {
            tracing::error!(error = %e, "failed to enqueue plugin command");
        }
    }

    /// 子プロセス sync の唯一の口 (docs/plan_arch_refactor.md §7.5 「sync 一本化」)。
    /// `edit_epoch` が前回 sync から進んでいるときだけ 6 段 choreography を実行し、
    /// 末尾で `last_synced_epoch` を現 epoch に更新する (choreography 内の
    /// `resolve_default_device_ports` normalize bump も吸収 = 1 frame で収束)。
    /// runner が frame 末に 1 回呼んで 1 frame 内の複数編集を 1 LoadSong に coalesce
    /// するほか、 編集直後に engine の最新 song 前提でコマンドを送る経路
    /// (Play / Seek / Export / PrepareVocalSynth 等) が送信直前に呼んで最新を先に
    /// 届ける (ensure-synced)。 epoch 一致時は即 return の no-op なので毎 frame・
    /// 毎コマンド前に呼んで安全。 旧 `sync_song_to_plugin_host` (無条件実行) +
    /// `flush_pending_host_sync` (`pending_host_sync` flag 経路) を吸収一本化した。
    /// `pub`: runner (frame flush) と各 handler (ensure-synced) のほか、 headless
    /// 統合テストが frame 境界を模して呼ぶ (`tests/app_state/*`)。
    ///
    /// r.md #131: 読み込み中の device の表 ([`Self::sync_loading_devices`]) もここで揃える — 増える分は `LoadSong` の
    /// 前、減る分は後。構造が変わらない frame でも表だけは揃える (読み込みの確定は Song を変えない)。
    pub fn flush_song_sync(&mut self) {
        if self.cur.song_doc.sync_epoch() == self.cur.pipc.last_synced_epoch {
            self.sync_loading_devices(false);
            return;
        }
        // v23 (review fix #4/#5/#6): daw_audio は各 device の役割を `ports` から
        // 位置導出する。旧 v22 project は load 直後 ports が default(全 false) で、
        // LoadSong 前に DB から解決しておかないと全 device が Inactive になり
        // 楽器が無音 / group FX が bypass される。picker 追加や SlotPluginLoaded で
        // 既に解決済みの device は ports != default なので skip され、steady state
        // では bool 比較だけで安い。この単一 chokepoint で全 load 経路を保護する。
        self.resolve_default_device_ports();
        // PR6: project_dir も送る (audio engine は AudioSourcePath::
        // ProjectRelative を解決するために必要、 §9.2)。 send_audio は
        // 順序保証付きの IPC なので SetProjectDir → LoadSong の順で
        // 送れば audio side の LoadSong handler 内で project_dir が
        // 既に最新になっている。
        let project_dir: Option<PathBuf> = self
            .cur.song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        self.send_audio(AudioCommand::SetProjectDir { project: self.pk(), dir: project_dir });
        let song = self.cur.song_doc.song().clone();
        // マスター音量は engine 側では atomic (`EngineShared.master_gain`) が live
        // 値を持つので、**送る Song から導いて**必ず一緒に届ける。ここは
        // 「Song が差し替わる」全経路 (Open / New / Undo / Redo / 復旧) が通る
        // 唯一の口なので、開いた直後に保存値が効かない取りこぼしが構造的に無い。
        self.send_audio(AudioCommand::SetMasterGain { project: self.pk(), gain: song.master_gain });
        self.sync_loading_devices(true);
        self.send_audio(AudioCommand::LoadSong { project: self.pk(), song });
        self.sync_loading_devices(false);
        // PR-V3: vocal track が builtin VOICEVOX を instrument に持つ場合、
        // notes / bpm 変更を plugin に flush して背景 synth を trigger。
        // 既存 vocal block (= track.instrument is None の旧 project) には
        // 影響しない (= sync_vocal_metadata 内で format check で skip)。
        self.sync_vocal_metadata();
        // (r.md #5 ARA2) ARA device を持つトラックの audio クリップを ARA document
        // として plugin host に公開する (差分があるときだけ送信)。
        self.sync_ara_documents();
        // 口パク自動再生成 (binding 済み vocal track のみ、debounce 付き)。
        self.mark_lipsync_dirty();
        // choreography 完了。 現 epoch を synced ベースラインにする
        // (resolve_default_device_ports の normalize bump も含めて吸収する
        // ため末尾で読む = 次 frame で epoch 一致 → no-op に収束)。
        self.cur.pipc.last_synced_epoch = self.cur.song_doc.sync_epoch();
    }

    /// r.md #131: engine が compile に使う「読み込み中の device」を `pending_plugin_loads` (所有者) に揃える
    /// (前に送った集合と違うときだけ `SetLoadingDevices` を送る)。engine はこれを持つトラックをグラフに入れないので、
    /// 有効に戻した / 開いた直後のトラックは読み込みが確定 (成功 / 失敗) した瞬間から鳴る。
    ///
    /// 順序が音を決める — engine は登録の無い device を素通しにするので:
    /// - **増える分は構造より前** (`grow_only = true`、前に送った集合 ∪ いまの読み込み中): `LoadSong` がトラックを
    ///   実行に入れる前に「読み込み中」を知っていないと、その間 FX の掛かっていない音が鳴る。
    /// - **減る分は構造より後** (`grow_only = false`): 消えた device (トラックの削除 / 無効化で読み込みを取り消した) を
    ///   それがまだ居る古い構造の上で外すと、一瞬素通しで鳴る。engine が今の構造を持っている (`LoadSong` の直後か、
    ///   送っていない編集が無い) ときだけ呼ぶ。読み込みが確定した 1 台は [`Self::settle_loading_device`]。
    pub(crate) fn sync_loading_devices(&mut self, grow_only: bool) {
        let pending = self.cur.pipc.pending_plugin_loads.keys().copied();
        let next: std::collections::BTreeSet<u64> = if grow_only {
            pending.chain(self.cur.pipc.last_sent_loading_devices.iter().copied()).collect()
        } else {
            pending.collect()
        };
        self.send_loading_devices(next);
    }

    /// r.md #131: 読み込みが確定した (`SlotPluginLoaded` / `SlotPluginLoadFailed`) 1 台だけを engine の「読み込み中」から
    /// 外す。成功なら `OpenPluginShmem` を送った後に呼ぶ (登録より先に外すと、登録が届くまで素通しで鳴る)。失敗なら
    /// その device は素通しになる (失敗した device の規則)。ほかの差分 (まだ送っていない編集で消えた device) は
    /// 構造と一緒に [`Self::flush_song_sync`] が揃える — ここで外すと古い構造の上で素通しになる。
    pub(crate) fn settle_loading_device(&mut self, device_id: u64) {
        let mut next = self.cur.pipc.last_sent_loading_devices.clone();
        if next.remove(&device_id) {
            self.send_loading_devices(next);
        }
    }

    fn send_loading_devices(&mut self, next: std::collections::BTreeSet<u64>) {
        if next == self.cur.pipc.last_sent_loading_devices {
            return;
        }
        let device_ids = next.iter().copied().collect();
        self.send_audio(AudioCommand::SetLoadingDevices { project: self.pk(), device_ids });
        self.cur.pipc.last_sent_loading_devices = next;
    }

    /// (r.md #5 ARA2) Expose each ARA-capable device's track audio clips to the
    /// plug-in as an ARA document. Diffs against [`Self::ara_doc_cache`] so
    /// `SetupAraDocument` (which reinitialises the plug-in) is sent only when the
    /// resolved clip set changes, and `ClearAraDocument` for slots no longer ARA.
    pub(crate) fn sync_ara_documents(&mut self) {
        let Some(db) = self.ipc.plugin_db.clone() else {
            self.clear_all_ara_documents();
            return;
        };
        let project_dir: Option<PathBuf> = self
            .cur.song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let bpm = f64::from(self.cur.song_doc.song().bpm).max(1.0);

        // (v29 §2) ARA track が参照する in-memory (`Generated`) source を
        // 先に WAV へ materialize する (旧 `AraSourceSpec::Pcm` の置換)。
        // collect (下の &self ループ) が `ara_pcm_materialized` から path を
        // 引けるように、 &mut self が要るこの pre-pass で済ませる。
        self.materialize_generated_sources_for_ara(&db);

        // Resolve the current ARA clip set for every ARA device (v29:
        // 安定 device_id keyed)。
        let mut live: std::collections::HashMap<u64, Vec<common::protocol::AraClipSpec>> =
            std::collections::HashMap::new();
        // r.md #131: 無効トラックの ARA device は host に居ない (document を組まない。cache から外れた分は下の stale)。
        let song = self.cur.song_doc.song();
        for track in song.tracks.iter().filter(|t| song.track_effectively_enabled(t.id)) {
            for device in track.plugins() {
                if device.id == 0
                    || !db.find_by_id(&device.plugin_id).is_some_and(|entry| entry.is_ara())
                {
                    continue;
                }
                let clips = self.collect_ara_clips_for_track(track, project_dir.as_deref(), bpm);
                live.insert(device.id, clips);
            }
        }

        /// Two resolved ARA clip sets have the same graph (same regions on the same
        /// modifications and sources, in order) — only their placement / stretch
        /// may differ, so the device can be updated in place.
        fn ara_same_clip_set(
            a: &[common::protocol::AraClipSpec],
            b: &[common::protocol::AraClipSpec],
        ) -> bool {
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| {
                    x.region_key == y.region_key
                        && x.modification_id == y.modification_id
                        && x.source_id == y.source_id
                        && x.source_wav == y.source_wav
                })
        }
        fn ara_region_update_of(
            clip: &common::protocol::AraClipSpec,
        ) -> common::protocol::AraRegionUpdate {
            common::protocol::AraRegionUpdate {
                region_key: clip.region_key.clone(),
                placement: clip.placement,
            }
        }

        // Diff against the cache (before any &mut self send), splitting changes
        // into in-place region updates and full rebuilds. A device whose clip set
        // is unchanged but whose placement / stretch differs is updated via
        // `UpdateAraRegions` — `updatePlaybackRegionProperties` is safe while
        // rendering, so live tempo / edge-drag follow doesn't interrupt playback.
        // A device that is new or whose clip set changed is rebuilt.
        let mut rebuilds: Vec<(u64, Vec<common::protocol::AraClipSpec>)> = Vec::new();
        let mut updates: Vec<(u64, Vec<common::protocol::AraRegionUpdate>)> = Vec::new();
        for (key, clips) in &live {
            match self.cur.pipc.ara_doc_cache.get(key) {
                Some(prev) if prev == clips => {}
                Some(prev) if ara_same_clip_set(prev, clips) => {
                    updates.push((*key, clips.iter().map(ara_region_update_of).collect()));
                }
                _ => rebuilds.push((*key, clips.clone())),
            }
        }
        let stale: Vec<u64> = self
            .cur.pipc.ara_doc_cache
            .keys()
            .filter(|key| !live.contains_key(*key))
            .copied()
            .collect();

        self.cur.pipc.ara_doc_cache = live;
        for (device_id, clips) in rebuilds {
            // The saved ARA edits for this device: the host restores them only into
            // the objects this update creates (objects already in the document keep
            // their live edits), reading legacy persistent ids through the aliases.
            let (archive, archive_ids) = self
                .cur.song_doc
                .song()
                .plugin_by_id(device_id)
                .map(|d| (d.ara_archive.as_deref().map(<[u8]>::to_vec), d.ara_archive_ids.clone()))
                .unwrap_or_default();
            self.send_plugin(PluginCommand::SetupAraDocument {
                device: self.dev(device_id),
                clips,
                bpm,
                time_sig: (self.cur.song_doc.song().time_sig.0 as u16, self.cur.song_doc.song().time_sig.1 as u16),
                archive,
                archive_ids,
            });
        }
        for (device_id, regions) in updates {
            self.send_plugin(PluginCommand::UpdateAraRegions { device: self.dev(device_id), regions });
        }
        for device_id in stale {
            self.send_plugin(PluginCommand::ClearAraDocument { device: self.dev(device_id) });
        }
    }

    /// Send `ClearAraDocument` for every cached ARA device and empty the cache.
    pub(crate) fn clear_all_ara_documents(&mut self) {
        let stale: Vec<u64> = self.cur.pipc.ara_doc_cache.keys().copied().collect();
        self.cur.pipc.ara_doc_cache.clear();
        for device_id in stale {
            self.send_plugin(PluginCommand::ClearAraDocument { device: self.dev(device_id) });
        }
    }

    /// (v29 §2) 旧 `AraSourceSpec::Pcm` の置換: ARA device を持つ track の
    /// audio event が参照する `AudioSourcePath::Generated` (in-memory) source
    /// を、 app cache dir (`<app_dirs.root>/ara_pcm/ara_pcm_<hash>.wav`) へ
    /// interleaved f32 WAV として書き出し、 path を
    /// `ara_pcm_materialized` に登録する。 既に登録済み / ファイル既存なら
    /// no-op (Generated source は immutable)。 decoded buffer が GUI cache に
    /// 無い source は書けないので skip (= 従来どおり ARA に出さない)。
    ///
    /// 呼び出しは UI thread の song-sync 経路 (非 RT)。 1 source につき
    /// 1 回限りの書き出しなので同期で書く (bounce 済み in-memory audio が
    /// wire を渡って 16MB 上限を破る旧設計の置換、 `docs/plan_arch_refactor.md` §2)。
    pub(crate) fn materialize_generated_sources_for_ara(&mut self, db: &PluginDatabase) {
        use common::model::{AudioSourcePath, ClipContent};
        // 対象: ARA device を持つ track の audio event が参照する Generated source。
        let mut todo: Vec<common::model::AudioSourceId> = Vec::new();
        let song = self.cur.song_doc.song();
        for track in song.tracks.iter().filter(|t| song.track_effectively_enabled(t.id)) {
            let has_ara = track
                .plugins()
                .any(|d| db.find_by_id(&d.plugin_id).is_some_and(|e| e.is_ara()));
            if !has_ara {
                continue;
            }
            for clip in &track.clips {
                let Some(ClipContent::Audio(audio)) =
                    self.cur.song_doc.song().clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                for event in &audio.events {
                    let Some(source) = self.cur.song_doc.song().media.audio_sources.get(&event.source_id) else {
                        continue;
                    };
                    if matches!(source.path, AudioSourcePath::Generated { .. })
                        && !self.cur.pipc.ara_pcm_materialized.contains_key(&event.source_id)
                    {
                        todo.push(event.source_id);
                    }
                }
            }
        }
        todo.sort_unstable();
        todo.dedup();
        for source_id in todo {
            if let Err(e) = self.materialize_generated_source(source_id) {
                tracing::warn!(source_id, error = %e, "ARA: failed to materialize generated source");
            }
        }
    }

    /// 1 つの Generated source を WAV に書き出して `ara_pcm_materialized` に
    /// 登録する。 buffer 未 decode / app_dirs 無しは Err。
    pub(crate) fn materialize_generated_source(
        &mut self,
        source_id: common::model::AudioSourceId,
    ) -> anyhow::Result<()> {
        use std::hash::Hasher as _;
        let Some(buffer) = self.cur.media.audio_source_cache.get(source_id) else {
            anyhow::bail!("generated source {source_id} has no decoded buffer in the GUI cache");
        };
        let Some(dirs) = self.ui_prefs.app_dirs.as_ref() else {
            anyhow::bail!("app data dir unavailable");
        };
        // content hash (FNV-1a 相当は std に無いので DefaultHasher で代用 —
        // ファイル名の安定性は「同 session 同 content で同名」 が要件)。
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write_u32(buffer.sample_rate);
        hasher.write_u16(buffer.channels);
        hasher.write_u64(buffer.frames);
        for plane in &buffer.samples {
            for &s in plane {
                hasher.write_u32(s.to_bits());
            }
        }
        let hash = hasher.finish();
        let dir = dirs.root().join("ara_pcm");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("ara_pcm_{hash:016x}.wav"));
        if !path.exists() {
            let spec = hound::WavSpec {
                channels: buffer.channels.max(1),
                sample_rate: buffer.sample_rate.max(1),
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut writer = hound::WavWriter::create(&path, spec)?;
            let frames = buffer.frames as usize;
            let channels = buffer.samples.len().max(1);
            for frame in 0..frames {
                for ch in 0..channels {
                    let s = buffer
                        .samples
                        .get(ch)
                        .and_then(|plane| plane.get(frame))
                        .copied()
                        .unwrap_or(0.0);
                    writer.write_sample(s)?;
                }
            }
            writer.finalize()?;
            tracing::info!(source_id, path = %path.display(), "ARA: materialized generated source to WAV");
        }
        self.cur.pipc.ara_pcm_materialized.insert(source_id, path);
        Ok(())
    }

    /// (r.md #5 ARA2) Resolve a track's audio clips into ARA clip specs: one
    /// playback region per **piece shown in a clip's window** (r.md #132 残件 —
    /// an event hidden outside the window is not heard, and the window's edges
    /// crop the region), on the audio modification of its content and take and
    /// the audio source of its file (`common::ara_ids`). Times convert from
    /// beats to seconds (ARA playback time is in seconds). File sources resolve
    /// to an absolute path; `Generated` resolves to its materialized WAV.
    pub(crate) fn collect_ara_clips_for_track(
        &self,
        track: &common::model::Track,
        project_dir: Option<&Path>,
        bpm: f64,
    ) -> Vec<common::protocol::AraClipSpec> {
        use common::model::ClipContent;
        let song = self.cur.song_doc.song();
        let mut out = Vec::new();
        for clip in &track.clips {
            let Some(ClipContent::Audio(audio)) = song.clip_contents.get(&clip.content_id) else {
                continue;
            };
            let (lo, hi) = clip.content_window();
            for i in common::model::shown_indices(&audio.events, (lo, hi)) {
                let event = &audio.events[i];
                let Some((source_wav, sample_rate)) = self.ara_source_wav(event.source_id, project_dir) else {
                    continue;
                };
                let (start, end) = (event.event_start_in_clip_beats, event.event_start_in_clip_beats + event.event_length_beats);
                let piece = common::model::event_piece(event, start.max(lo), end.min(hi));
                out.push(common::protocol::AraClipSpec {
                    source_wav,
                    source_id: common::ara_ids::source_id(event.source_id),
                    modification_id: common::ara_ids::modification_id(clip.content_id, event),
                    modification_origin: common::ara_ids::modification_origin(song, clip.content_id, event),
                    region_key: common::ara_ids::region_key(clip.id, event.id),
                    placement: ara_region_placement(clip, &piece, sample_rate, bpm),
                });
            }
        }
        out
    }

    /// ARA に渡す素材 `source_id` の絶対 WAV path と sample rate。 解決できなければ `None`。
    fn ara_source_wav(
        &self,
        source_id: common::model::AudioSourceId,
        project_dir: Option<&Path>,
    ) -> Option<(PathBuf, f64)> {
        use common::model::AudioSourcePath;
        let source = self.cur.song_doc.song().media.audio_sources.get(&source_id)?;
        let path = match &source.path {
            AudioSourcePath::Absolute(p) => p.clone(),
            AudioSourcePath::ProjectRelative(rel) => project_dir?.join(rel),
            // (v29 §2) in-memory audio は wire に載せず、 事前に
            // `materialize_generated_sources_for_ara` が書き出した
            // WAV path を渡す (未 materialize = decoded buffer 無し
            // は従来どおり skip)。
            AudioSourcePath::Generated { .. } => self.cur.pipc.ara_pcm_materialized.get(&source_id)?.clone(),
        };
        Some((path, f64::from(source.sample_rate).max(1.0)))
    }

    /// v23 (review fix): `ports` が未解決 (全 false) の device を plugin DB
    /// から解決する。旧 project load 直後の device は ports を持たないため、
    /// LoadSong 前にこれを呼ばないと daw_audio の役割導出が全 Inactive になり
    /// 無音になる。解決の規則は [`PortConfig::resolve`] が SSoT で、
    /// `SlotPluginLoaded` の backfill (`handler/devices.rs`) と同じものを使う。
    pub(crate) fn resolve_default_device_ports(&mut self) {
        let Some(db) = self.ipc.plugin_db.clone() else {
            return;
        };
        // 先に読みだけで解決の要否を判定する (steady state では bool 比較のみ)。
        // 解決は「ユーザー編集」ではない正規化なので normalize (undo 履歴に
        // 入れない。 epoch は進む = 子プロセス sync は走り、 2 周目は
        // 解決済みで no-op に収束する)。
        let needs = {
            let song = self.cur.song_doc.song();
            song.all_plugins()
                .any(|d| d.ports.is_unresolved() && db.find_by_id(&d.plugin_id).is_some())
        };
        if !needs {
            return;
        }
        // r.md #9: no-op 検出付き normalize。 解決が実際に ports を書き換えたとき
        // だけ epoch を bump する (既に解決済み / 解決結果が同じなら dirty 化
        // しない)。 旧 project の port 解決は本当に内容が変わる migration なので、
        // そのときは dirty で正しい (= 再保存で解決済み ports を永続化)。
        self.normalize_song_checked(|song| {
            let mut changed = false;
            song.for_each_plugin_mut(&mut |d| {
                let resolved = common::port_config::PortConfig::resolve(
                    d.ports,
                    db.find_by_id(&d.plugin_id).map(port_config_of),
                );
                if resolved != d.ports {
                    d.ports = resolved;
                    changed = true;
                }
            });
            changed
        });
    }

}

/// (r.md #5 ARA2) audio event 1 つの ARA region の置き方 (秒)。
///
/// 見せるのは event の **窓** (r.md #132 残件: 分割の片は take の一部): modification の範囲は take の
/// 写像で窓に当たる source の区間、playback は窓の song 位置と長さ。 Raw は source を native rate で
/// そのまま鳴らす (playback 長 = modification 長、窓の長さで打ち切る)。 それ以外の mode は take の伸縮率で
/// 窓の source 区間を窓の拍の長さへ time-stretch する (手の端 drag / テンポ変更も同じ式に流れる。 #6 と対)。
fn ara_region_placement(
    clip: &common::model::Clip,
    event: &common::model::AudioEvent,
    sample_rate: f64,
    bpm: f64,
) -> common::protocol::AraRegionPlacement {
    let secs_per_beat = 60.0 / bpm;
    let time_stretch = event.stretch_mode != common::model::StretchMode::Raw;
    let window_start = event.source_start_frames as f64 / sample_rate;
    let playback_secs = event.event_length_beats * secs_per_beat;
    let (start_in_modification, duration_in_modification) = if time_stretch {
        let frames_per_beat = event.take_frames_per_beat(sample_rate * secs_per_beat);
        (
            window_start + event.take_head_beats * frames_per_beat / sample_rate,
            event.event_length_beats * frames_per_beat / sample_rate,
        )
    } else {
        let head_secs = event.take_head_beats * secs_per_beat;
        let remaining = event.source_window_frames() as f64 / sample_rate - head_secs;
        (window_start + head_secs, remaining.min(playback_secs).max(0.0))
    };
    common::protocol::AraRegionPlacement {
        // r.md #44: event の song 位置は content 原点基準。
        start_in_playback_seconds: clip.content_to_song_beat(event.event_start_in_clip_beats) * secs_per_beat,
        duration_in_playback_seconds: if time_stretch { playback_secs } else { duration_in_modification },
        start_in_modification_seconds: start_in_modification,
        duration_in_modification_seconds: duration_in_modification,
        time_stretch,
    }
}
