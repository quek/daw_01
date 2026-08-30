//! handler::bounce — clip 位置設定 + bounce (in-place / with-fx / 分離 render)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use std::path::{PathBuf};
use std::sync::{Arc};
use common::model::{AudioEvent, Song};
use common::protocol::{AudioCommand, PluginCommand};
use crate::import_audio;

impl AppData {
    pub(crate) fn set_clip_positions(&mut self, entries: &[(ClipKey, u32, f64)]) {
        // track 跨ぎ move: source track と to_track が異なれば clip を remove +
        // 別 track に再 push。 同 track 内なら start_beat だけ update。
        // 同 track 内で複数 entry がある場合、 高い clip_idx から処理しないと
        // 配列インデックスが先に変動してしまうので、 source.track 同一 group
        // ごとに clip_idx 降順で sort してから処理する。
        let mut entries: Vec<(ClipKey, u32, f64)> = entries.to_vec();
        entries.sort_by(|a, b| {
            a.0.track_id
                .cmp(&b.0.track_id)
                .then_with(|| b.0.clip_id.cmp(&a.0.clip_id))
        });

        let follow_automation = self.ui_prefs.automation_follows_clips;
        let Some(new_refs) = self.edit_song(move |song| {
            // **まず動かすクリップを全部外し、外し終えてから置く。** 1 つずつ
            // 「外して置く」 を繰り返すと、先に置いたクリップが *まだ動いていない仲間* を
            // 非重なり規則で削ってしまう (ブロック選択をまとめてドラッグすると多発する)。
            let mut moving: Vec<(u32, bool, common::model::Clip)> =
                Vec::with_capacity(entries.len());
            for (source, to_track_id, new_start_beat) in entries {
                let same_track = source.track_id == to_track_id;
                let old_window = song.clip_by_key(source).map(common::model::Clip::song_window);
                let Some((mut removed, _)) = song
                    .track_by_id_mut(source.track_id)
                    .and_then(|t| t.remove_clip_by_id(source.clip_id))
                else {
                    continue;
                };
                let new_start = new_start_beat.max(0.0);
                // **オートメーション追従** (`docs/plan_range_selection.md` §5)。
                // 同じトラック内の移動だけが対象 — トラックを跨ぐと、移動先の
                // どのレーンへ移すかが一意に決まらない。
                if follow_automation
                    && same_track
                    && let Some((old_start, old_end)) = old_window
                {
                    move_track_automation(song, source.track_id, old_start, old_end, new_start - old_start);
                }
                removed.start_beat = new_start;
                moving.push((to_track_id, same_track, removed));
            }
            let mut new_refs: Vec<(u32, u32)> = Vec::with_capacity(moving.len());
            for (to_track_id, same_track, mut clip) in moving {
                let Some(to_track_idx) = song.track_index_by_id(to_track_id) else {
                    continue;
                };
                // トラックを跨ぐときは id を採り直す (移動先の別クリップと
                // 衝突しうるため)。同トラック内なら id を保って選択を安定させる。
                if !same_track {
                    clip.id = 0;
                }
                let Some(to_track) = song.tracks.get_mut(to_track_idx) else {
                    continue;
                };
                let new_clip_id = to_track.place_clip(clip);
                new_refs.push((to_track_idx as u32, new_clip_id));
            }
            new_refs
        }) else {
            return;
        };
        // 新 clip 群を包む範囲を選択にする (stable ClipKey で解決)。
        let keys: Vec<common::model::ClipKey> = new_refs
            .iter()
            .filter_map(|(t_idx, c_id)| {
                let track = self.song_doc.song().tracks.get(*t_idx as usize)?;
                track
                    .clips
                    .iter()
                    .any(|c| c.id == *c_id)
                    .then_some(common::model::ClipKey {
                        track_id: track.id,
                        clip_id: *c_id,
                    })
            })
            .collect();
        self.select_new_clips(&keys);
    }

    /// Bounce In Place (Pre-FX、 `docs/plan_audio_clip.md` §3.8 / §13 Q8)。
    /// `target` clip 内の全 events を engine sample_rate で stereo mix
    /// して WAV 32-bit float ファイルに書き出し、 新 `AudioSource` を
    /// 採番して `Song.audio_sources` に insert、 `audio_source_cache` に
    /// 登録、 `ClipContent::Audio { events: [単一新 event] }` で置換、
    /// audio engine に `SetGeneratedAudio` で配信する。 同 `ContentId` を
    /// 共有していた linked clip も新 content で同期される (= `clip_contents`
    /// は `ContentId` 単位の pool)。
    ///
    /// 出力先: project_dir があれば `<project_dir>/bounce/<name>_<ts>.wav`、
    /// 未保存 project は `%LOCALAPPDATA%/daw_01/bounce_cache/<filename>.wav`
    /// (= `import_cache` と同じ fallback、 save 時に
    /// `migrate_unsaved_bounce_sources_into` が `<project_dir>/bounce/` へ
    /// 移動 + path を ProjectRelative 化する)。
    ///
    /// Pre-FX なので plugin chain (instrument / fx_chain) は通さない。
    /// source の events を fade / gain / pan / pitch_ratio で mix した
    /// snapshot のみ。 plugin 効果込みの bounce は spec §3.8 "Bounce"
    /// (= 新 Clip + 新 track) で別 PR。
    /// bounce 用に「対象クリップの 1 トラックだけ」を残した Song を組む。
    /// 他トラック・`master_fx_chain`・group/send/sidechain 参照を全て落とすので、engine の
    /// offline render はそのトラック単独の音だけを焼く (= clip isolate、 他トラックが
    /// 混ざらない)。`bypass_inserts == true` (Bounce In Place) のとき、残すトラックの
    /// insert FX device (= `ports.has_audio_input`) を `PortConfig::default()` で中和して
    /// 「音源/synth の素の音」だけにする。**device は削除しない**: engine は plugin を
    /// 安定 `device_id` で解決するので、device を消すと host 側 instance との対応が
    /// 切れる。並びを保ったまま ports を空にして dispatch を無害化する。元トラックの mute も解除する
    /// (= 元トラックが with-FX bounce で mute 済みでも isolate render は鳴らす)。
    pub(crate) fn isolated_bounce_song(&self, target: ClipKey, bypass_inserts: bool) -> Option<Song> {
        let track = self.song_doc.song().track_by_id(target.track_id)?;
        let mut isolated = self.song_doc.song().clone();
        isolated.master_fx_chain.clear();
        // master fx と同じ理由でマスター音量も外す。焼き込むと、再生時に
        // マスターフェーダーが**もう一度**掛かって二重に効く。
        // (master_gain が Song に入る前は engine の atomic 経由で漏れていた。)
        isolated.master_gain = 1.0;
        let mut kept = track.clone();
        kept.parent_group_id = None;
        kept.sends.clear();
        kept.muted = false;
        kept.solo = false;
        for d in &mut kept.devices {
            d.aux_inputs.clear();
            if bypass_inserts && d.ports.has_audio_input {
                d.ports = common::port_config::PortConfig::default();
            }
        }
        isolated.tracks = vec![kept];
        Some(isolated)
    }

    /// bounce 出力 WAV の path と `AudioSourcePath` を決める。保存済み
    /// project は `<dir>/bounce/<name>[_fx]_<ts>.wav`、未保存は bounce_cache (save 時に
    /// `migrate_unsaved_bounce_sources_into` が project へ移動 + ProjectRelative 化)。
    /// With FX は suffix `_fx` で In Place と区別する。失敗時は status_message を立てて `None`。
    pub(crate) fn bounce_output_path(
        &mut self,
        clip_name: &str,
        mode: BounceMode,
    ) -> Option<(PathBuf, common::model::AudioSourcePath)> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 % 100_000_000)
            .unwrap_or(0);
        let safe_name: String = clip_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let safe_name = if safe_name.is_empty() { "bounce".into() } else { safe_name };
        let infix = match mode {
            BounceMode::InPlace => "",
            BounceMode::WithFx => "_fx",
        };
        let filename = format!("{safe_name}{infix}_{ts:08}.wav");
        let project_dir = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        match project_dir.as_deref() {
            Some(dir) => {
                let bounce_dir = dir.join("bounce");
                if let Err(e) = std::fs::create_dir_all(&bounce_dir) {
                    self.ui_ephemeral.status_message = format!("Bounce: bounce/ 作成失敗: {e}");
                    return None;
                }
                Some((
                    bounce_dir.join(&filename),
                    common::model::AudioSourcePath::ProjectRelative(
                        std::path::PathBuf::from("bounce").join(&filename),
                    ),
                ))
            }
            None => {
                let cache = import_audio::unsaved_bounce_cache_dir();
                if let Err(e) = std::fs::create_dir_all(&cache) {
                    self.ui_ephemeral.status_message = format!("Bounce: bounce_cache/ 作成失敗: {e}");
                    return None;
                }
                let dst = cache.join(&filename);
                Some((dst.clone(), common::model::AudioSourcePath::Absolute(dst)))
            }
        }
    }

    /// bounce のトリガ共通処理。対象クリップ 1 トラックだけを isolate した
    /// song を engine に LoadSong し、offline render を要求する。In Place は insert FX を
    /// バイパス (port 中和)、With FX は insert FX を通す。結果は完了通知 handler
    /// (`handle_bounce_clip_fx_complete`) が mode に応じて「同位置置換」/「新トラック +
    /// 元ミュート」する。Audio / MIDI / 歌唱クリップが対象 (= 旧 is-Audio guard を撤去し
    /// 「全く無反応」 を解消)。完了通知の `flush_song_sync` が full song を再
    /// LoadSong して engine state を復元する。歌唱の合成待ちは `request_bounce` が前段で行う。
    pub(crate) fn start_clip_bounce(&mut self, target: ClipKey, mode: BounceMode) {
        if self.ipc.pending_clip_fx_bounce.is_some() {
            self.ui_ephemeral.status_message = "Bounce: 既に bounce 中です。 完了をお待ちください".into();
            return;
        }
        let Some(track) = self.song_doc.song().track_by_id(target.track_id) else {
            return;
        };
        let source_track_id = track.id;
        let Some(clip) = track.clip_by_id(target.clip_id).cloned() else {
            return;
        };
        let clip_name = self.song_doc.song().content_name(clip.content_id).to_string();
        // bounce 可能なのは Audio / Midi (= 歌唱含む) のみ。Automation/Video/Image/Text は対象外。
        if !matches!(
            self.song_doc.song().clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Midi(_) | common::model::ClipContent::Audio(_))
        ) {
            self.ui_ephemeral.status_message = "Bounce: audio / MIDI / 歌唱クリップのみ対象です".into();
            return;
        }
        // r.md #54: 範囲は **拍のまま** engine へ送る (拍→サンプル換算は
        // `beats_to_samples` = tempo automation を積分する SSoT 一本)。定数 BPM で
        // ここで換算していた旧形は、テンポカーブのある曲で bounce した WAV の
        // 長さが clip 長とずれた。
        let start_beat = clip.start_beat.max(0.0);
        let end_beat = clip.start_beat + clip.length_beats;
        if end_beat <= start_beat {
            self.ui_ephemeral.status_message = "Bounce: clip 長が 0 です".into();
            return;
        }
        let Some((out_path, source_path)) = self.bounce_output_path(&clip_name, mode) else {
            return;
        };
        let Some(isolated) = self.isolated_bounce_song(target, mode == BounceMode::InPlace) else {
            return;
        };
        self.ipc.pending_clip_fx_bounce = Some(PendingClipFxBounce {
            mode,
            source_track: target.track_id,
            source_clip: target.clip_id,
            source_track_id,
            source_content_id: clip.content_id,
            out_path: out_path.clone(),
            source_path,
            clip_name: clip_name.clone(),
            clip_length_beats: clip.length_beats,
            start_beat: clip.start_beat,
            content_offset_beats: clip.content_offset_beats,
        });
        // SetRenderMode(Offline) → LoadSong(isolated) → BounceClipFxOnline。完了通知で
        // Realtime に戻し、restore_engine_song_after_bounce が full song を再 LoadSong
        // して復元する。 この isolated 送出は epoch flush とは独立の明示経路 (isolated は
        // song_doc の編集ではないので edit_epoch は変わらず、 last_synced_epoch も
        // 触らない → frame flush は no-op のままで isolated を上書きしない)。
        // engine のマスター音量は atomic なので、送る Song に合わせて明示的に
        // 揃える (bounce 中は unity)。「engine が持つ song と master_gain は常に
        // 一致する」を LoadSong の全送出点で保つ。
        self.send_audio(AudioCommand::SetMasterGain(isolated.master_gain));
        self.send_audio(AudioCommand::LoadSong(isolated));
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(AudioCommand::BounceClipFxOnline {
            path: out_path,
            source_track: target.track_id,
            source_clip: target.clip_id,
            start_beat,
            end_beat,
        });
        let label = match mode {
            BounceMode::InPlace => "Bounce In Place",
            BounceMode::WithFx => "Bounce (with FX)",
        };
        self.ui_ephemeral.status_message = format!("{label}: '{clip_name}' を render 中...");
    }

    /// In Place = 音源/synth の素の音 (insert FX 抜き) を engine offline
    /// render で焼き、**同じクリップに置換** (async)。歌唱の合成待ちは `request_bounce` 経由。
    pub(crate) fn bounce_clip_in_place(&mut self, target: ClipKey) {
        self.request_bounce(target, BounceMode::InPlace);
    }

    /// track の builtin VOICEVOX device の安定 device id を返す
    /// (`sync_vocal_metadata` と同じ解決)。device 未挿入 / load 未確定
    /// (load 完了通知前 = `loaded_devices` に居ない) なら `None`。
    pub(crate) fn vocal_builtin_plugin_id(&self, track: &common::model::Track) -> Option<u64> {
        track
            .devices
            .iter()
            .find(|d| {
                d.format == common::plugin_format::PluginFormat::Builtin
                    && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
            })
            .map(|d| d.id)
            .filter(|id| self.ipc.loaded_devices.contains_key(id))
    }

    /// bounce の入口。歌唱トラックは合成が非同期 HTTP で走り、 offline render が
    /// 合成完了前に終わると無音になるため、 metadata を flush して `PrepareVocalSynth` を
    /// 送り、 plugin host の `VocalSynthReady`（builtin の synth 世代が最新メタデータまで
    /// 進んだ通知）を待ってから `start_clip_bounce` する。歌唱以外 (Audio / 通常 MIDI)、
    /// または plugin_id 未確定なら即 `start_clip_bounce`。
    pub(crate) fn request_bounce(&mut self, target: ClipKey, mode: BounceMode) {
        if self.ipc.pending_clip_fx_bounce.is_some() || self.ipc.pending_vocal_synth_bounce.is_some() {
            self.ui_ephemeral.status_message = "Bounce: 既に bounce 中です。 完了をお待ちください".into();
            return;
        }
        // 歌唱トラック + builtin plugin_id 解決済み → 合成完了を待ってから render。
        // 待ち中の編集で index が動いても追跡できるよう stable id で退避する。
        let vocal = self
            .song_doc.song()
            .track_by_id(target.track_id)
            .filter(|t| t.is_voicevox_vocal())
            .and_then(|t| {
                let plugin_id = self.vocal_builtin_plugin_id(t)?;
                let clip_id = t.clip_by_id(target.clip_id)?.id;
                Some((plugin_id, t.id, clip_id))
            });
        if let Some((device_id, track_id, clip_id)) = vocal {
            self.ipc.pending_vocal_synth_bounce =
                Some(PendingVocalSynthBounce { track_id, clip_id, mode });
            // r.md #27: bounce は合成完了 (`VocalSynthReady`) を待つので、metadata が
            // 前回送信と不変でも必ず再送して synth 世代を進める。差分キャッシュを迂回
            // するため該当 device の entry を落としてから flush する (= 直前の合成が
            // engine 未起動等で失敗していても bounce で確実に再試行される)。
            self.voicevox.voicevox_metadata_sent.remove(&device_id);
            self.sync_vocal_metadata();
            self.send_plugin(PluginCommand::PrepareVocalSynth { device_id });
            self.ui_ephemeral.status_message = "Bounce: 歌唱を合成中...".into();
            return;
        }
        self.start_clip_bounce(target, mode);
    }

    /// PR-C: plugin chain 込みで render し、 結果を **新 track + 新 Clip**
    /// に配置 (`docs/plan_audio_followup.md` PR-C / `docs/plan_audio_clip
    /// .md` §3.8 "Bounce")。 Bounce In Place (Pre-FX) と異なり async (=
    /// IPC 経由で freewheel render 完了通知待ち)。 完了通知の handler
    /// (`handle_bounce_clip_fx_complete`) 内で Undo snapshot を 1 回だけ
    /// 取る。 既に bounce 進行中なら重複 request を拒否。
    /// With FX = 音源/synth + そのトラックの insert FX を engine offline
    /// render で焼き、**新トラックに複製** + 元トラック自動ミュート (非破壊・二重再生
    /// 回避、async)。対象クリップ 1 トラックだけを isolate するので他トラックは混ざらない
    /// (旧実装は時間範囲の全ミックスを焼くバグがあった)。歌唱の合成待ちは `request_bounce` 経由。
    pub(crate) fn bounce_clip_with_fx(&mut self, target: ClipKey) {
        self.request_bounce(target, BounceMode::WithFx);
    }

    /// bounce 完了/失敗時に、 `start_clip_bounce` が `LoadSong(isolated)` で退避させた
    /// audio engine の song を full song へ戻す。 これは epoch flush とは独立の明示
    /// 直接 send: isolated 送出も restore も edit_epoch を動かさないので
    /// `flush_song_sync` は no-op (epoch 一致) のまま = 自力で full song を送り直さ
    /// ないと engine が isolate された 1 トラックのままになる。 vocal / ARA 等の派生
    /// 同期は不要 (song 内容は bounce 前と同一)。
    pub(crate) fn restore_engine_song_after_bounce(&mut self) {
        let song = self.song_doc.song().clone();
        // bounce 中に unity へ落としたマスター音量も一緒に戻す。
        self.send_audio(AudioCommand::SetMasterGain(song.master_gain));
        self.send_audio(AudioCommand::LoadSong(song));
    }

    /// PR-C: BounceClipFxOnline 完了通知の処理。 SetRenderMode(Realtime)
    /// で bookend 解除、 success なら新 audio source + 新 track + 新
    /// audio clip を配置 + Undo snapshot。 失敗時は pending クリア + 残骸
    /// ファイル削除 + full song 再 LoadSong (= engine の isolated song を復元)。
    pub(crate) fn handle_bounce_clip_fx_complete(
        &mut self,
        path: PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    ) {
        let Some(pending) = self.ipc.pending_clip_fx_bounce.take() else {
            // 対応する pending が無い completion (respawn 後の残骸等)。 render mode
            // だけ防御的に Realtime へ戻す。
            self.send_plugin(PluginCommand::SetRenderMode(
                common::protocol::RenderMode::Realtime,
            ));
            tracing::warn!("BounceClipFxComplete with no pending bounce; ignoring");
            return;
        };
        if pending.source_track != source_track
            || pending.source_clip != source_clip
            || pending.out_path != path
        {
            tracing::warn!(
                ?path,
                source_track,
                source_clip,
                "BounceClipFxComplete identifier mismatch with pending; ignoring"
            );
            // 進行中の本命 bounce の追跡 (と Offline render mode) は壊さない。
            self.ipc.pending_clip_fx_bounce = Some(pending);
            return;
        }
        // bookend を Realtime に戻す (= 失敗時も忘れず)。
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));
        let label = match pending.mode {
            BounceMode::InPlace => "Bounce In Place",
            BounceMode::WithFx => "Bounce (with FX)",
        };
        if let Some(err) = error {
            self.ui_ephemeral.status_message = format!("{label} 失敗: {err}");
            let _ = std::fs::remove_file(&path);
            self.restore_engine_song_after_bounce();
            return;
        }
        if frames == 0 {
            self.ui_ephemeral.status_message =
                format!("{label}: render 結果が空です (= silence のみ?)");
            let _ = std::fs::remove_file(&path);
            self.restore_engine_song_after_bounce();
            return;
        }
        // InPlace の置換対象 content が bounce 中の編集で消えていたら結果を破棄
        // (index でなく stable id で判定。 別クリップを誤置換しない)。
        if pending.mode == BounceMode::InPlace
            && !self.song_doc.song().clip_contents.contains_key(&pending.source_content_id)
        {
            self.ui_ephemeral.status_message =
                "Bounce In Place: 対象クリップが消えたため結果を破棄しました".into();
            let _ = std::fs::remove_file(&path);
            self.restore_engine_song_after_bounce();
            return;
        }

        // 1 完了 = 1 Undo step として snapshot を取る。

        let engine_sr = self.ipc.sample_rate;
        // 採番した new_source_id を `audio_sources` に登録。 path は
        // `pending.source_path` (= ProjectRelative or Absolute、 確定済)。
        let new_source = common::model::AudioSource {
            path: pending.source_path,
            sample_rate: engine_sr,
            channels: 2,
            frames,
            original_bpm: Some(self.song_doc.song().bpm),
            root_key: None,
        };
        let Some(new_source_id) = self.edit_song(move |song| {
            let new_source_id = song.alloc_audio_source_id();
            song.media.audio_sources.insert(new_source_id, new_source);
            new_source_id
        }) else {
            return;
        };

        // decode して audio_source_cache に登録 (= 即時再生で playback
        // できるよう)。 失敗しても tracker 表示等は問題ないので warn だけ。
        match crate::import_audio::decode_audio(&path) {
            Ok(buffer) => {
                self.media.audio_source_cache.insert(new_source_id, Arc::new(buffer));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Bounce (with FX): WAV decode for cache failed (track is created; will reload on next save/load)"
                );
            }
        }

        // 新 Clip / 置換に使う共通 audio event (single-event = bounce 結果は flat な audio)。
        // v29: 新規 content の単一 event なので id=1 / allocator は 2 から。
        let new_event = AudioEvent {
            id: 1,
            source_id: new_source_id,
            // r.md #44: 元 clip の窓の起点に置く (In Place 置換で窓と一致させる)。
            // With FX の新 clip は窓 offset をそのまま引き継ぐ。
            event_start_in_clip_beats: pending.content_offset_beats,
            event_length_beats: pending.clip_length_beats,
            source_start_frames: 0,
            source_end_frames: frames,
            ..AudioEvent::default()
        };

        match pending.mode {
            BounceMode::WithFx => {
                // 新 track 作成 (空 plugin chain)。 名前は元 clip 名 + " (FX)"。
                let Some(new_track_id) = self.edit_song(|song| song.alloc_track_id()) else {
                    return;
                };
                let new_track_name = format!("{} (FX)", pending.clip_name);
                let new_track = track_with(|t| {
                    t.id = new_track_id;
                    t.name = new_track_name.clone();
                    t.clips = Vec::new();
                });
                self.edit_song(|song| song.tracks.push(new_track));
                let new_track_idx = self.song_doc.song().tracks.len() - 1;

                let bounced_content_name = format!("{} (bounced FX)", pending.clip_name);
                let Some(new_content_id) = self.edit_song(move |song| {
                    song.alloc_content(
                        common::model::ClipContent::Audio(common::model::AudioContent {
                            events: vec![new_event],
                            next_event_id: 2,
                        }),
                        bounced_content_name,
                    )
                }) else {
                    return;
                };

                self.edit_song(|song| {
                    let new_track_mut = &mut song.tracks[new_track_idx];
                    new_track_mut.place_clip(common::model::Clip {
                        id: 0,
                        start_beat: pending.start_beat,
                        length_beats: pending.clip_length_beats,
                        content_id: new_content_id,
                        content_offset_beats: pending.content_offset_beats,
                        color: None,
                        auto_lipsync: false,
                        ..Default::default()
                    });

                    // 二重再生回避のため元トラックを自動ミュート。 別 SetTrackMuted は
                    // 不要 (下の flush_song_sync が muted=true 込みの full song を LoadSong)。
                    // index は bounce 中の編集で stale になり得るので stable id で解決する
                    // (削除済みなら skip = 二重再生の危険自体が無い)。
                    if let Some(src) =
                        song.tracks.iter_mut().find(|t| t.id == pending.source_track_id)
                    {
                        src.muted = true;
                    }
                });

                self.resize_track_peak_display();
                self.ui_ephemeral.status_message = format!(
                    "Bounce (with FX) 完了: 新トラック '{new_track_name}' を追加 (元トラックはミュート)",
                );
            }
            BounceMode::InPlace => {
                // 元クリップの content を bounce 結果 (single audio event) に
                // 置換 (= flat 化)。 同 content_id を共有する linked clip も追従する。
                // 対象は bounce 開始時に捕捉した stable な content id (index 経由の
                // 再解決は bounce 中の編集でずれる)。 存在は上で検証済み。
                self.edit_song(move |song| {
                    if let Some(content) =
                        song.clip_contents.get_mut(&pending.source_content_id)
                    {
                        *content = common::model::ClipContent::Audio(common::model::AudioContent {
                            events: vec![new_event],
                            next_event_id: 2,
                        });
                    }
                });
                self.ui_ephemeral.status_message = format!("Bounce In Place 完了: '{}'", pending.clip_name);
            }
        }
    }

}

/// **オートメーション追従**: `[start, end)` に居た automation を `delta` 拍ずらす。
///
/// 境界でクリップを割ってから、区間に完全に入るものだけを動かす。 移動先は上書き規則で
/// 削ってから置く (automation クリップも `Track.clips` と同じ非重なり不変条件を持つ)。
/// **閉じているレーンも含めて**そのトラックの全レーンに効く (設定が ON のときだけ呼ばれる)。
fn move_track_automation(
    song: &mut common::model::Song,
    track_id: u32,
    start: f64,
    end: f64,
    delta: f64,
) {
    const EPS: f64 = 1e-9;
    if delta.abs() <= EPS || end <= start + EPS {
        return;
    }
    let Some(track) = song.track_by_id_mut(track_id) else {
        return;
    };
    for lane in &mut track.automation_lanes {
        lane.split_at(start);
        lane.split_at(end);
        let mut moving: Vec<common::model::AutomationClip> = Vec::new();
        lane.clips.retain(|c| {
            let inside = c.start_beat >= start - EPS && c.start_beat + c.length_beats <= end + EPS;
            if inside {
                moving.push(c.clone());
            }
            !inside
        });
        if moving.is_empty() {
            continue;
        }
        lane.carve_clip_range(start + delta, end + delta, None);
        for mut c in moving {
            c.start_beat += delta;
            lane.clips.push(c);
        }
    }
}
