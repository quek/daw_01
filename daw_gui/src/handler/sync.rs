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
        tracing::info!(?msg, "sending to audio");
        let Some(tx) = self.ipc.audio_tx.as_ref() else {
            tracing::warn!("audio sender is not configured");
            return;
        };
        if let Err(e) = tx.send(msg) {
            tracing::error!(error = %e, "failed to enqueue audio command");
        }
    }

    pub(crate) fn send_plugin(&self, msg: PluginCommand) {
        tracing::info!(?msg, "sending to plugin_host");
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
    pub fn flush_song_sync(&mut self) {
        if self.song_doc.edit_epoch() == self.ipc.last_synced_epoch {
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
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        self.send_audio(AudioCommand::SetProjectDir(project_dir));
        let song = self.song_doc.song().clone();
        self.send_audio(AudioCommand::LoadSong(song));
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
        self.ipc.last_synced_epoch = self.song_doc.edit_epoch();
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
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let bpm = f64::from(self.song_doc.song().bpm).max(1.0);

        // (v29 §2) ARA track が参照する in-memory (`Generated`) source を
        // 先に WAV へ materialize する (旧 `AraSourceSpec::Pcm` の置換)。
        // collect (下の &self ループ) が `ara_pcm_materialized` から path を
        // 引けるように、 &mut self が要るこの pre-pass で済ませる。
        self.materialize_generated_sources_for_ara(&db);

        // Resolve the current ARA clip set for every ARA device (v29:
        // 安定 device_id keyed)。
        let mut live: std::collections::HashMap<u64, Vec<common::protocol::AraClipSpec>> =
            std::collections::HashMap::new();
        for track in &self.song_doc.song().tracks {
            for device in track.devices.iter() {
                if device.id == 0
                    || !db.find_by_id(&device.plugin_id).is_some_and(|entry| entry.is_ara())
                {
                    continue;
                }
                let clips = self.collect_ara_clips_for_track(track, project_dir.as_deref(), bpm);
                live.insert(device.id, clips);
            }
        }

        /// Two resolved ARA clip sets have the same regions (same persistent_id
        /// and source, in order) — only their placement / stretch may differ, so
        /// the device can be updated in place instead of rebuilt.
        fn ara_same_clip_set(
            a: &[common::protocol::AraClipSpec],
            b: &[common::protocol::AraClipSpec],
        ) -> bool {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(x, y)| x.persistent_id == y.persistent_id && x.source_wav == y.source_wav)
        }
        fn ara_region_update_of(
            clip: &common::protocol::AraClipSpec,
        ) -> common::protocol::AraRegionUpdate {
            common::protocol::AraRegionUpdate {
                persistent_id: clip.persistent_id.clone(),
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
            match self.ipc.ara_doc_cache.get(key) {
                Some(prev) if prev == clips => {}
                Some(prev) if ara_same_clip_set(prev, clips) => {
                    updates.push((*key, clips.iter().map(ara_region_update_of).collect()));
                }
                _ => rebuilds.push((*key, clips.clone())),
            }
        }
        let stale: Vec<u64> = self
            .ipc.ara_doc_cache
            .keys()
            .filter(|key| !live.contains_key(*key))
            .copied()
            .collect();

        self.ipc.ara_doc_cache = live;
        for (device_id, clips) in rebuilds {
            // Restore any saved ARA edits for this device alongside the rebuild.
            let archive = self
                .song_doc.song()
                .tracks
                .iter()
                .flat_map(|t| t.devices.iter())
                .find(|d| d.id == device_id)
                .and_then(|d| d.ara_archive.as_deref().map(<[u8]>::to_vec));
            self.send_plugin(PluginCommand::SetupAraDocument {
                device_id,
                clips,
                bpm,
                time_sig: (self.song_doc.song().time_sig.0 as u16, self.song_doc.song().time_sig.1 as u16),
                archive,
            });
        }
        for (device_id, regions) in updates {
            self.send_plugin(PluginCommand::UpdateAraRegions { device_id, regions });
        }
        for device_id in stale {
            self.send_plugin(PluginCommand::ClearAraDocument { device_id });
        }
    }

    /// Send `ClearAraDocument` for every cached ARA device and empty the cache.
    pub(crate) fn clear_all_ara_documents(&mut self) {
        let stale: Vec<u64> = self.ipc.ara_doc_cache.keys().copied().collect();
        self.ipc.ara_doc_cache.clear();
        for device_id in stale {
            self.send_plugin(PluginCommand::ClearAraDocument { device_id });
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
        for track in &self.song_doc.song().tracks {
            let has_ara = track
                .devices
                .iter()
                .any(|d| db.find_by_id(&d.plugin_id).is_some_and(|e| e.is_ara()));
            if !has_ara {
                continue;
            }
            for clip in &track.clips {
                let Some(ClipContent::Audio(audio)) =
                    self.song_doc.song().clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                for event in &audio.events {
                    let Some(source) = self.song_doc.song().audio_sources.get(&event.source_id) else {
                        continue;
                    };
                    if matches!(source.path, AudioSourcePath::Generated { .. })
                        && !self.ipc.ara_pcm_materialized.contains_key(&event.source_id)
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
        let Some(buffer) = self.media.audio_source_cache.get(source_id) else {
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
        self.ipc.ara_pcm_materialized.insert(source_id, path);
        Ok(())
    }

    /// (r.md #5 ARA2) Resolve a track's audio clips into ARA clip specs. Times
    /// convert from beats to seconds (ARA playback time is in seconds); the
    /// source slice maps 1:1 without time-stretch. File sources resolve to an
    /// absolute path; `Generated` (no on-disk file) is skipped for now.
    pub(crate) fn collect_ara_clips_for_track(
        &self,
        track: &common::model::Track,
        project_dir: Option<&Path>,
        bpm: f64,
    ) -> Vec<common::protocol::AraClipSpec> {
        use common::model::{AudioSourcePath, ClipContent};
        let mut out = Vec::new();
        for clip in &track.clips {
            let Some(ClipContent::Audio(audio)) = self.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            for (event_index, event) in audio.events.iter().enumerate() {
                let Some(source) = self.song_doc.song().audio_sources.get(&event.source_id) else {
                    continue;
                };
                let abs = match &source.path {
                    AudioSourcePath::Absolute(p) => p.clone(),
                    AudioSourcePath::ProjectRelative(rel) => match project_dir {
                        Some(dir) => dir.join(rel),
                        None => continue,
                    },
                    // (v29 §2) in-memory audio は wire に載せず、 事前に
                    // `materialize_generated_sources_for_ara` が書き出した
                    // WAV path を渡す (未 materialize = decoded buffer 無し
                    // は従来どおり skip)。
                    AudioSourcePath::Generated { .. } => {
                        match self.ipc.ara_pcm_materialized.get(&event.source_id) {
                            Some(p) => p.clone(),
                            None => continue,
                        }
                    }
                };
                let sample_rate = f64::from(source.sample_rate).max(1.0);
                let start_in_modification = event.source_start_frames as f64 / sample_rate;
                let duration_in_modification = event
                    .source_end_frames
                    .saturating_sub(event.source_start_frames)
                    as f64
                    / sample_rate;
                let start_in_playback =
                    (clip.start_beat + event.event_start_in_clip_beats) * 60.0 / bpm;
                // Raw plays the slice natively (no stretch): playback duration ==
                // modification duration. Every other mode follows the clip's
                // timeline length, so the playback duration is the event's beat
                // span in seconds (event_length_beats × 60/bpm) and the plug-in
                // pitch-preservingly time-stretches the slice onto it. Manual
                // edge-drag changes event_length_beats; a tempo change changes
                // bpm — both flow through here (mirrors #6 for non-ARA audio).
                let time_stretch = event.stretch_mode != common::model::StretchMode::Raw;
                let duration_in_playback = if time_stretch {
                    event.event_length_beats * 60.0 / bpm
                } else {
                    duration_in_modification
                };
                out.push(common::protocol::AraClipSpec {
                    source_wav: abs,
                    persistent_id: format!("{}:{}:{event_index}", event.source_id, clip.id),
                    placement: common::protocol::AraRegionPlacement {
                        start_in_playback_seconds: start_in_playback,
                        duration_in_playback_seconds: duration_in_playback,
                        start_in_modification_seconds: start_in_modification,
                        duration_in_modification_seconds: duration_in_modification,
                        time_stretch,
                    },
                });
            }
        }
        out
    }

    /// v23 (review fix): `ports` が default (全 false) の device を plugin DB
    /// から解決する。旧 project load 直後の device は ports を持たないため、
    /// LoadSong 前にこれを呼ばないと daw_audio の役割導出が全 Inactive になり
    /// 無音になる。既に解決済み (= いずれかの port が true) の device は触らない
    /// (= picker 追加 / SlotPluginLoaded backfill 済みは no-op、DB 不在も保持)。
    pub(crate) fn resolve_default_device_ports(&mut self) {
        let Some(db) = self.ipc.plugin_db.clone() else {
            return;
        };
        let default_ports = common::port_config::PortConfig::default();
        // 先に読みだけで解決の要否を判定する (steady state では bool 比較のみ)。
        // 解決は「ユーザー編集」ではない正規化なので normalize (undo 履歴に
        // 入れない。 epoch は進む = 子プロセス sync は走り、 2 周目は
        // ports != default で no-op に収束する)。
        let needs = {
            let song = self.song_doc.song();
            song.tracks
                .iter()
                .flat_map(|t| t.devices.iter())
                .chain(song.master_fx_chain.iter())
                .any(|d| d.ports == default_ports && db.find_by_id(&d.plugin_id).is_some())
        };
        if !needs {
            return;
        }
        self.song_doc.normalize(|song| {
            let resolve = |devices: &mut [common::model::PluginInstance]| {
                for d in devices.iter_mut() {
                    if d.ports == default_ports
                        && let Some(entry) = db.find_by_id(&d.plugin_id)
                    {
                        d.ports = port_config_of(entry);
                    }
                }
            };
            for track in song.tracks.iter_mut() {
                resolve(&mut track.devices);
            }
            resolve(&mut song.master_fx_chain);
        });
    }

}
