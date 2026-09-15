//! handler::bounce — bounce (in-place / with-fx / 分離 render)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use std::path::{PathBuf};
use std::sync::{Arc};
use common::protocol::{AudioCommand, PluginCommand, RenderScope};
use crate::import_audio;

impl AppData {
    /// render 出力 WAV の path と `AudioSourcePath` を決める。保存済み
    /// project は `<dir>/bounce/<name><infix>_<ts>.wav`、未保存は bounce_cache (save 時に
    /// `finish_save` が project へ移動 + ProjectRelative 化)。
    /// `infix` は種別の区別 (In Place = `""` / With FX = `"_fx"` / Glue = `"_glue"`)。
    /// 失敗時は status_message を立てて `None`。
    ///
    /// **名前は必ず未使用のものを返す。** 一意化が「サニタイズ済みクリップ名 + ミリ秒」
    /// だけだった頃は、Glue が同名トラックを **同じミリ秒内に**連続採番するので
    /// 2 トラックが同じ WAV を掴み、後の render が前の render を上書きして
    /// 「別トラックの音が鳴る」形で無言に壊れた (同名 clip / 無名 clip / 非 ASCII 名は
    /// サニタイズで潰れて同名になるので、通常操作で普通に踏む)。
    pub(crate) fn bounce_output_path(
        &mut self,
        clip_name: &str,
        infix: &str,
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
        let project_dir = self
            .cur.song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        // **空ファイルを作って名前を予約する。** `exists()` を見るだけでは足りない —
        // Glue は render を 1 本も走らせる前に全 job の path を採番するので、
        // まだ誰もファイルを書いていない時点で同じ名前を 2 回返してしまう。
        // `create_new` は「無いときだけ作る」ので、同一ミリ秒でも他プロセスとでも衝突しない
        // (render の `WavWriter::create` がこの空ファイルを上書きする)。
        let unique_in = |dir: &std::path::Path| -> String {
            let mut n = 0u32;
            loop {
                let filename = if n == 0 {
                    format!("{safe_name}{infix}_{ts:08}.wav")
                } else {
                    format!("{safe_name}{infix}_{ts:08}_{n}.wav")
                };
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join(&filename))
                {
                    Ok(_) => return filename,
                    // 予約できない (権限等) なら名前だけ返す — render 側が同じ理由で
                    // 失敗して status に出る。無限ループにはしない。
                    Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return filename,
                    Err(_) => n += 1,
                }
            }
        };
        match project_dir.as_deref() {
            Some(dir) => {
                let bounce_dir = dir.join("bounce");
                if let Err(e) = std::fs::create_dir_all(&bounce_dir) {
                    self.ui_ephemeral.status_message = format!("Bounce: bounce/ 作成失敗: {e}");
                    return None;
                }
                let filename = unique_in(&bounce_dir);
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
                let dst = cache.join(unique_in(&cache));
                Some((dst.clone(), common::model::AudioSourcePath::Absolute(dst)))
            }
        }
    }

    /// bounce のトリガ共通処理。対象クリップ 1 トラックだけを isolate した
    /// song ([`common::model::Song::isolated_track`]) を engine に LoadSong し、offline render を要求する。
    /// In Place は素材の音 (`RenderScope::Sources`)、With FX は device チェーンまで (`RenderScope::PostFx`) を焼く。
    /// 結果は完了通知 handler (`handle_bounce_clip_fx_complete`) が mode に応じて「同位置置換」/
    /// 「新トラック + 元クリップのミュート」([`common::model::Song::place_bounce_with_fx`]) する。
    /// Audio / MIDI / 歌唱クリップが対象 (= 旧 is-Audio guard を撤去し「全く無反応」 を解消)。完了通知の `flush_song_sync` が full song を再
    /// LoadSong して engine state を復元する。歌唱の合成待ちは `request_bounce` が前段で行う。
    /// `label` = 発注した操作の履歴ラベル (完了時の 1 undo step の名前)。
    pub(crate) fn start_clip_bounce(&mut self, target: ClipKey, mode: BounceMode, label: &'static str) {
        // 書き出し / 解析 / Glue の焼き込みも同じ offline render を使う (engine は同時 1 本)。
        if self.refuse_render_while_another("Bounce") {
            return;
        }
        // 歌唱の合成待ち (`request_bounce`) の間に plugin を足していれば、焼き始める瞬間にもう一度待つ
        // (確定したら `request_bounce` からやり直す = 合成が最新かも確かめ直す)。
        if self.plugin_loads_pending() {
            return self.defer_render(PendingRender::Bounce { target, mode, label });
        }
        let Some(track) = self.cur.song_doc.song().track_by_id(target.track_id) else {
            return;
        };
        let source_track_id = track.id;
        let Some(clip) = track.clip_by_id(target.clip_id).cloned() else {
            return;
        };
        let clip_name = self.cur.song_doc.song().content_name(clip.content_id).to_string();
        // bounce 可能なのは Audio / Midi (= 歌唱含む) のみ。Automation/Video/Image/Text は対象外。
        if !matches!(
            self.cur.song_doc.song().clip_contents.get(&clip.content_id),
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
        let infix = match mode {
            BounceMode::InPlace => "",
            BounceMode::WithFx => "_fx",
        };
        let Some((out_path, source_path)) = self.bounce_output_path(&clip_name, infix) else {
            return;
        };
        let Some(isolated) = self.cur.song_doc.song().isolated_track(target.track_id) else {
            return;
        };
        // In Place は元のクリップを置き換える = 焼いた音が再生時にトラックの fx / フェーダーをもう一度通るので、
        // 素材の音だけを焼く。With FX は別トラックに置いて元をミュートし、フェーダーとそこから先は元トラックから
        // 写すので、device チェーンまで (`common::model::bounce_ops`)。どちらも master の段は通さない
        // (再生時に master をもう一度通る)。
        let scope = match mode {
            BounceMode::InPlace => RenderScope::Sources,
            BounceMode::WithFx => RenderScope::PostFx,
        };
        self.cur.pipc.pending_clip_fx_bounce = Some(PendingClipFxBounce {
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
            label,
        });
        // SetRenderMode(Offline) → LoadSong(isolated) → BounceClipFxOnline。完了通知で
        // Realtime に戻し、restore_engine_song_after_bounce が full song を再 LoadSong
        // して復元する。 この isolated 送出は epoch flush とは独立の明示経路 (isolated は
        // song_doc の編集ではないので edit_epoch は変わらず、 last_synced_epoch も
        // 触らない → frame flush は no-op のままで isolated を上書きしない)。
        // engine のマスター音量は atomic なので、送る Song に合わせて明示的に
        // 揃える (bounce は master の段を通さない scope なので値は効かない)。「engine が持つ song と
        // master_gain は常に一致する」を LoadSong の全送出点で保つ。
        self.send_audio(AudioCommand::SetMasterGain { project: self.pk(), gain: isolated.master_gain });
        self.send_audio(AudioCommand::LoadSong { project: self.pk(), song: isolated });
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(AudioCommand::BounceClipFxOnline {
            project: self.pk(),
            path: out_path,
            source_track: target.track_id,
            source_clip: target.clip_id,
            start_beat,
            end_beat,
            // clip bounce は plugin 状態 (tail / ramp / sidechain) を積み上げてから
            // 範囲に入る必要があるので曲頭から走る。
            warm: true,
            scope,
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
        let label = self.cur.song_doc.event_label();
        self.request_bounce(target, BounceMode::InPlace, label);
    }

    /// track の builtin VOICEVOX device の安定 device id を返す
    /// (`sync_vocal_metadata` と同じ解決)。device 未挿入 / load 未確定
    /// (load 完了通知前 = `loaded_devices` に居ない) なら `None`。
    pub(crate) fn vocal_builtin_plugin_id(&self, track: &common::model::Track) -> Option<u64> {
        track
            .plugins()
            .find(|d| {
                d.format == common::plugin_format::PluginFormat::Builtin
                    && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
            })
            .map(|d| d.id)
            .filter(|id| self.cur.pipc.loaded_devices.contains_key(id))
    }

    /// bounce の入口。歌唱トラックは合成が非同期 HTTP で走り、 offline render が
    /// 合成完了前に終わると無音になるため、 metadata を flush して `PrepareVocalSynth` を
    /// 送り、 plugin host の `VocalSynthReady`（builtin の synth 世代が最新メタデータまで
    /// 進んだ通知）を待ってから `start_clip_bounce` する。歌唱以外 (Audio / 通常 MIDI)、
    /// または plugin_id 未確定なら即 `start_clip_bounce`。
    ///
    /// plugin の読み込みが残っていれば、歌唱かどうかを決める前に確定を待つ (`PendingRender::Bounce`、読み込み待ち
    /// からの再開もここ) — 読み込み中の VOICEVOX は `vocal_builtin_plugin_id` に出ないので、待たずに決めると合成を
    /// 待たずに焼く。`label` = 発注した操作の履歴ラベル (完了時の 1 undo step の名前)。
    pub(crate) fn request_bounce(&mut self, target: ClipKey, mode: BounceMode, label: &'static str) {
        if self.refuse_render_while_another("Bounce") {
            return;
        }
        // r.md #131: 無効なトラックは実行系に居ない (plugin も host から降りている) ので焼けない。焼くと無音 /
        // 素通しの音でクリップを置き換えてしまう。
        if !self.cur.song_doc.song().track_effectively_enabled(target.track_id) {
            self.ui_ephemeral.status_message = "Bounce: 無効なトラックは焼けません (有効にしてから)".into();
            return;
        }
        if self.plugin_loads_pending() {
            return self.defer_render(PendingRender::Bounce { target, mode, label });
        }
        // 歌唱トラック + builtin plugin_id 解決済み → 合成完了を待ってから render。
        // 待ち中の編集で index が動いても追跡できるよう stable id で退避する。
        let vocal = self
            .cur.song_doc.song()
            .track_by_id(target.track_id)
            .filter(|t| t.is_voicevox_vocal())
            .and_then(|t| {
                let plugin_id = self.vocal_builtin_plugin_id(t)?;
                let clip_id = t.clip_by_id(target.clip_id)?.id;
                Some((plugin_id, t.id, clip_id))
            });
        if let Some((device_id, track_id, clip_id)) = vocal {
            self.cur.pipc.pending_vocal_synth_bounce =
                Some(PendingVocalSynthBounce { track_id, clip_id, mode, label });
            // r.md #27: bounce は合成完了 (`VocalSynthReady`) を待つので、metadata が
            // 前回送信と不変でも必ず再送して synth 世代を進める。差分キャッシュを迂回
            // するため該当 device の entry を落としてから flush する (= 直前の合成が
            // engine 未起動等で失敗していても bounce で確実に再試行される)。
            self.cur.pvv.voicevox_metadata_sent.remove(&device_id);
            self.sync_vocal_metadata();
            self.send_plugin(PluginCommand::PrepareVocalSynth { device: self.dev(device_id) });
            self.ui_ephemeral.status_message = "Bounce: 歌唱を合成中...".into();
            return;
        }
        self.start_clip_bounce(target, mode, label);
    }

    /// PR-C: plugin chain 込みで render し、 結果を **新 track + 新 Clip**
    /// に配置 (`docs/plan_audio_followup.md` PR-C / `docs/plan_audio_clip
    /// .md` §3.8 "Bounce")。 Bounce In Place (Pre-FX) と異なり async (=
    /// IPC 経由で freewheel render 完了通知待ち)。 完了通知の handler
    /// (`handle_bounce_clip_fx_complete`) 内で Undo snapshot を 1 回だけ
    /// 取る。 既に bounce 進行中なら重複 request を拒否。
    /// With FX = 音源/synth + そのトラックの device チェーン (内蔵 device 含む) を engine offline
    /// render で焼き、**新トラックに置いてフェーダー / send / 行き先を写す** + 元クリップ自動ミュート
    /// (非破壊・二重再生回避、async、規則は [`common::model::Song::place_bounce_with_fx`])。対象クリップ 1 トラックだけを
    /// isolate するので他トラックは混ざらない
    /// (旧実装は時間範囲の全ミックスを焼くバグがあった)。歌唱の合成待ちは `request_bounce` 経由。
    pub(crate) fn bounce_clip_with_fx(&mut self, target: ClipKey) {
        let label = self.cur.song_doc.event_label();
        self.request_bounce(target, BounceMode::WithFx, label);
    }

    /// bounce 完了/失敗時に、 `start_clip_bounce` が `LoadSong(isolated)` で退避させた
    /// audio engine の song を full song へ戻す。 これは epoch flush とは独立の明示
    /// 直接 send: isolated 送出も restore も edit_epoch を動かさないので
    /// `flush_song_sync` は no-op (epoch 一致) のまま = 自力で full song を送り直さ
    /// ないと engine が isolate された 1 トラックのままになる。 ARA 等の派生同期は不要
    /// (song 内容は bounce 前と同一)。**歌唱のメタデータだけは送り直す** — 焼いている間は書いた音
    /// (移調 0、r.md #130 Q8) で送っていたので、移調込みへ戻す (差分キャッシュの比較で、移調していない曲や
    /// 歌唱でない bounce では何も送らない)。
    pub(crate) fn restore_engine_song_after_bounce(&mut self) {
        let song = self.cur.song_doc.song().clone();
        // LoadSong の全送出点でマスター音量を Song に揃える (bounce 側の送出と同じ規則)。
        self.send_audio(AudioCommand::SetMasterGain { project: self.pk(), gain: song.master_gain });
        self.send_audio(AudioCommand::LoadSong { project: self.pk(), song });
        self.sync_vocal_metadata();
    }

    /// いま **焼いている** (歌唱の合成待ち / offline render 中の) トラック。そのトラックの歌唱メタデータは書いた音
    /// (移調 0) で送る (r.md #130 Q8: 焼いた音はもう一度移調に追従する)。
    pub(crate) fn baking_vocal_track(&self) -> Option<u32> {
        let pipc = &self.cur.pipc;
        pipc.pending_vocal_synth_bounce
            .as_ref()
            .map(|p| p.track_id)
            .or_else(|| pipc.pending_clip_fx_bounce.as_ref().map(|p| p.source_track_id))
    }

    /// PR-C: BounceClipFxOnline 完了通知の処理。 SetRenderMode(Realtime)
    /// で bookend 解除、 success なら新 audio source を登録し、In Place は content を置換、
    /// With FX は新 track + 新 audio clip を配置 (1 Undo step)。 失敗時は pending クリア + 残骸
    /// ファイル削除 + full song 再 LoadSong (= engine の isolated song を復元)。
    pub(crate) fn handle_bounce_clip_fx_complete(
        &mut self,
        path: PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    ) {
        let Some(pending) = self.cur.pipc.pending_clip_fx_bounce.take() else {
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
            self.cur.pipc.pending_clip_fx_bounce = Some(pending);
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
        // 置き先が bounce 中の編集で消えていたら結果を破棄する (index でなく stable id で判定):
        // In Place は置換対象の content (別クリップを誤置換しない)、With FX は元クリップ
        // (mute する相手と、写すフェーダーの持ち主が居ない)。
        let song = self.cur.song_doc.song();
        let source = ClipKey { track_id: pending.source_track_id, clip_id: pending.source_clip };
        let gone = match pending.mode {
            BounceMode::InPlace => (!song.clip_contents.contains_key(&pending.source_content_id)).then_some("対象クリップ"),
            BounceMode::WithFx => song.clip_by_key(source).is_none().then_some("元クリップ"),
        };
        if let Some(what) = gone {
            self.ui_ephemeral.status_message = format!("{label}: {what}が消えたため結果を破棄しました");
            let _ = std::fs::remove_file(&path);
            self.restore_engine_song_after_bounce();
            return;
        }

        // 1 完了 = 1 Undo step。path は `pending.source_path` (= ProjectRelative or Absolute、確定済)。
        // r.md #44: event は元 clip の窓の起点に置く (In Place 置換で窓と一致させる)。
        // With FX の新 clip は窓 offset をそのまま引き継ぐ。
        let wav = common::model::BakedWav { path: pending.source_path.clone(), sample_rate: self.ipc.sample_rate, frames };
        let window = (pending.start_beat, pending.start_beat + pending.clip_length_beats);
        let (mode, offset, length) = (pending.mode, pending.content_offset_beats, pending.clip_length_beats);
        let source_content_id = pending.source_content_id;
        let track_name = format!("{} (FX)", pending.clip_name);
        let (content_name, new_track_name) = (format!("{} (bounced FX)", pending.clip_name), track_name.clone());
        // 発注した操作の名前で独立した 1 step (進行中のドラッグの bracket には入れない)。
        let gesture = self.cur.song_doc.enter_own_gesture(pending.label);
        let placed = self.edit_song(move |song| {
            let (source_id, content) = song.add_baked_audio(wav, window, offset);
            let content = common::model::ClipContent::Audio(content);
            match mode {
                BounceMode::WithFx => {
                    let content_id = song.alloc_content(content, content_name);
                    let clip = common::model::Clip {
                        start_beat: window.0,
                        length_beats: length,
                        content_id,
                        content_offset_beats: offset,
                        ..Default::default()
                    };
                    song.place_bounce_with_fx(source, new_track_name, clip)?;
                }
                // 元クリップの content を置換 (= flat 化)。同 content_id を共有する linked clip も追従する。
                BounceMode::InPlace => *song.clip_contents.get_mut(&source_content_id)? = content,
            }
            Some(source_id)
        });
        self.cur.song_doc.leave_own_gesture(gesture);
        let Some(Some(source_id)) = placed else {
            return;
        };

        // decode して audio_source_cache に登録 (= 即時再生で playback
        // できるよう)。 失敗しても tracker 表示等は問題ないので warn だけ。
        match crate::import_audio::decode_audio(&path) {
            Ok(buffer) => {
                self.cur.media.audio_source_cache.insert(source_id, Arc::new(buffer));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    label,
                    "bounce WAV decode for cache failed (will reload on next save/load)"
                );
            }
        }
        self.ui_ephemeral.status_message = match mode {
            BounceMode::WithFx => {
                self.resize_track_peak_display();
                format!("Bounce (with FX) 完了: 新トラック '{track_name}' を追加 (元クリップはミュート)")
            }
            BounceMode::InPlace => format!("Bounce In Place 完了: '{}'", pending.clip_name),
        };
    }

}
