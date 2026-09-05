//! handler::export — WAV/MIDI/MP4 export の range/実行 + file dialog
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use std::path::{Path, PathBuf};
use common::protocol::{AudioCommand, PluginCommand};

impl AppData {
    /// 動画書き出しの前段で作った **temp 一式** (WAV + その隣の sidecar) を消す。
    ///
    /// WAV だけ消して sidecar を残していた箇所が 3 つあり (完了 / 音声キャンセル /
    /// 音声失敗)、`%TEMP%\daw01_export_audio_{pid}.wav` は pid が同じ間ずっと同じ名前が
    /// 再利用されるので、消し残しがそのまま溜まっていた。「何を消すか」を 1 か所に
    /// 集めて、sidecar が増えたときに 3 箇所へ足し忘れないようにする。
    pub(crate) fn remove_export_temp_wav(&mut self) {
        let Some(wav) = self.transport.export_temp_wav.take() else {
            return;
        };
        let _ = std::fs::remove_file(
            common::mod_sidecar::ModEnvSidecar::sidecar_path(&wav),
        );
        let _ = std::fs::remove_file(
            common::launcher_sidecar::LauncherSidecar::sidecar_path(&wav),
        );
        let _ = std::fs::remove_file(&wav);
    }

    /// レンジピッカーを開くときの既定範囲 (拍)。 ループ範囲が設定されていれば
    /// それを既定にし、 無ければ全曲 (0..length_beats) にフォールバックする。
    /// ループ範囲は session state (`transport.loop_region`) が所有する SSoT で、
    /// transport の再生ループと同じ値を使う (= 「ループしている区間をそのまま
    /// 書き出す」 という DAW で一般的な既定)。 ON/OFF は見ない (帯が引いてあれば
    /// その範囲が「今の関心領域」)。 末尾は最低 `MIN_EXPORT_RANGE_BEATS` を保証する。
    pub(crate) fn default_export_range(&self) -> (f64, f64) {
        let (start, end) = self
            .transport
            .loop_region
            .range()
            .unwrap_or((0.0, self.song_doc.song().length_beats));
        let start = start.max(0.0);
        (start, end.max(start + MIN_EXPORT_RANGE_BEATS))
    }

    /// Export WAV / Video を押したときに、 まず書き出す **時間範囲**
    /// (拍) を選ぶレンジピッカーモーダルを開く。 デフォルト窓は `default_export_range`
    /// = ループ範囲 (設定されていれば) / 無ければ全曲。 確定 (`ConfirmExportRange`)
    /// で `kind` に応じた既存の export action (file dialog) を起動する。 Ardour /
    /// REAPER の time-selection export と同じ「範囲を指定して書き出す」 UX。
    pub(crate) fn open_export_range_picker(&mut self, kind: ExportRangeKind) {
        // video export は実行中だと二重起動できない (旧 action_open_export_mp4_dialog
        // のガードをここへ移設)。
        if matches!(kind, ExportRangeKind::Mp4)
            && (self.transport.export_stage.is_some()
                || self.transport.pending_video_export.is_some()
                || self.ui_ephemeral.export_dialog_open)
        {
            self.ui_ephemeral.status_message = "Video export を実行中です".into();
            return;
        }
        let (start_beat, end_beat) = self.default_export_range();
        self.ui_ephemeral.export_range_picker = Some(ExportRangePicker {
            start_beat,
            end_beat,
            kind,
            // 既定はプロジェクト現在値 (= 1920x1080 / 30)。 dropdown で
            // 変更した値は per-export override として確定時に運ばれる。
            resolution: self.song_doc.song().video_resolution,
            framerate: self.song_doc.song().video_framerate,
        });
    }

    /// レンジピッカー確定。 選んだ拍範囲を kind に応じて変換し、 元の
    /// export action を起動する。 「全曲」 (start=0, end=length) のときは範囲なし
    /// (`None`) として従来どおり全曲を書き出す。
    pub(crate) fn confirm_export_range(&mut self) {
        let Some(picker) = self.ui_ephemeral.export_range_picker.take() else {
            return;
        };
        // start=0 かつ end>=length は全曲とみなす (= None)。 浮動小数の比較は緩く。
        let is_full = picker.start_beat <= f64::EPSILON
            && picker.end_beat >= self.song_doc.song().length_beats - f64::EPSILON;
        let range_beats: Option<(f64, f64)> =
            if is_full { None } else { Some((picker.start_beat, picker.end_beat)) };
        match picker.kind {
            ExportRangeKind::Wav => {
                let dialog = rfd::FileDialog::new().add_filter("WAV", &["wav"]);
                self.spawn_file_dialog(
                    dialog,
                    FileDialogMode::Save,
                    FileDialogKind::ExportWav { range: range_beats },
                );
            }
            // r.md #54: ファイルを書かないのでダイアログ無しでそのまま走る。
            ExportRangeKind::Loudness => self.begin_loudness_analysis(range_beats),
            ExportRangeKind::Mp4 => {
                // picker で選んだ出力解像度 / fps を後段へ運ぶ。
                let resolution = picker.resolution;
                let framerate = picker.framerate;
                #[cfg(windows)]
                self.action_open_export_mp4_dialog(range_beats, resolution, framerate);
                #[cfg(not(windows))]
                {
                    let _ = (range_beats, resolution, framerate);
                    self.ui_ephemeral.status_message =
                        "Video export は Windows 専用 (WMF 経由) です".into();
                }
            }
        }
    }

    /// r.md #54: ワンクリック範囲プリセットを拍範囲へ解決する。
    /// 対象が存在しなければ `None` (ボタンを無効表示にする根拠にもなる)。
    ///
    /// 拍のまま返すのが要点 — 拍→サンプル換算は daw_audio の
    /// `beats_to_samples` (tempo automation を積分する SSoT) だけが行う。
    pub(crate) fn export_range_from_source(
        &self,
        source: ExportRangeSource,
    ) -> Option<(f64, f64)> {
        let song = self.song_doc.song();
        match source {
            ExportRangeSource::Loop => self.transport.loop_region.range(),
            // 通常クリップ面と automation 面のどちらで選んでいても拾う
            // (last-selection-wins の面判定はここでは不要 — 両方の bounding を取る)。
            ExportRangeSource::Selection => {
                match (
                    self.arrange_selection_beat_span(false),
                    self.arrange_selection_beat_span(true),
                ) {
                    (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                }
            }
            // プレイヘッドが乗っているセクション (無ければ最初のセクション)。
            ExportRangeSource::Section => {
                let at = f64::from(self.transport.playhead_beat.unwrap_or(0.0));
                song.sections
                    .iter()
                    .find(|s| at >= s.start_beat && at < s.end_beat())
                    .or_else(|| song.sections.first())
                    .map(|s| (s.start_beat, s.end_beat()))
            }
            ExportRangeSource::Whole => Some((0.0, song.length_beats)),
        }
        .filter(|(s, e)| e > s)
    }

    /// begin an offline WAV export the right way — stop playback,
    /// push the latest song + offline render mode, then **reinitialise every
    /// plugin** (deactivate→activate) for a clean cold render before the render
    /// runs. The actual `ExportWav` is sent on `AppEvent::PluginsReinitDone`
    /// (see the handler) once the plugin host confirms the reinit. Without the
    /// reinit a synth holding a live voice (VCV Rack 2) bleeds into the head;
    /// CLAP `reset()` alone does not clear it. Used by both the standalone WAV
    /// export and the video export's audio render.
    pub(crate) fn begin_wav_export(
        &mut self,
        path: std::path::PathBuf,
        range: Option<(f64, f64)>,
        write_mod_sidecar: bool,
    ) {
        // 書き出しは freewheel render。 先に停止する (live dispatch と export
        // dispatch が同じ plugin host worker slot で衝突するのを防ぐ)。
        // r.md #51: `is_playing` で条件を付けない。 これは観測値なので、走り始めた
        // 直後は false のことがあり、そこで Stop を送らないと engine が走ったまま
        // freewheel に入り、書き出し後に勝手に再生が続く。 stop() は録音セッションの
        // クローズも兼ねる (録音中の書き出しで Rec が点きっぱなしにならない)。
        self.stop();
        // ensure-synced: freewheel 開始前に最新 song + project_dir を daw_audio へ
        // 確実に届ける (SetRenderMode(Offline) より前)。 epoch 未変化 (= frame flush
        // 済) なら no-op で engine は既に最新を持つ。
        self.flush_song_sync();
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        // 全 plugin を deactivate→activate でクリーンにしてから export する。
        // 完了 (`PluginsReinitDone`) で stashed export を発火。
        self.transport.pending_export = Some((path, range, write_mod_sidecar));

        // r.md #75: 合成が終わる前に render すると部分ミックスが焼かれる (フレーズ単位で
        // 逐次 publish するようになったため)。全 VOICEVOX device に最新メタデータを
        // 流し直して完了を待つ。
        // **reinit より前**に待つ — deactivate は synth thread を止めるので、走っている
        // job があるとそこで捨てられ、`done_gen` が永久に追いつかない。
        let devices = self.all_vocal_synth_device_ids();
        if devices.is_empty() {
            self.send_plugin(PluginCommand::ReinitAllPlugins);
            return;
        }
        for &device_id in &devices {
            // bounce と同じ理由で差分キャッシュを迂回する (前回失敗していても再試行し、
            // 合成世代を必ず 1 つ進めて `VocalSynthReady` を確実に返させる)。
            self.voicevox.voicevox_metadata_sent.remove(&device_id);
        }
        self.sync_vocal_metadata();
        self.ipc.pending_vocal_synth_export = devices.iter().copied().collect();
        for device_id in devices {
            self.send_plugin(PluginCommand::PrepareVocalSynth { device_id });
        }
    }

    /// 子プロセス切断時に、進行中の bounce / 書き出しを畳む脱出口。中止したら `true`。
    ///
    /// bounce 進行中の crash では `BounceClipFxComplete` / `VocalSynthReady` が永遠に
    /// 来ない。pending を放置すると以後の bounce が全て「既に bounce 中」で拒否され、
    /// audio 側は isolated song のまま残る。`abort_audio_export` と同型の脱出口
    /// (どちらの子の crash でも安全に解除できる)。
    ///
    /// r.md #75 の合成完了ゲート (`pending_vocal_synth_export`) も同じ理由で畳む。
    pub(crate) fn abort_inflight_renders_on_disconnect(&mut self) -> bool {
        let mut aborted = false;
        // `J` (Glue) の焼き込みも同じ offline render を使う。 自前の後始末
        // (出力ファイルの削除 + bookend + engine song 復元) を持っているので、
        // pending を落とすだけでなくそちらへ委ねる。
        if self.ipc.pending_glue_bake.is_some() {
            self.abort_glue_bake(
                "音声エンジンが切断されたため Glue の焼き込みを中止しました".into(),
            );
            aborted = true;
        }
        if self.ipc.pending_clip_fx_bounce.take().is_some()
            || self.ipc.pending_vocal_synth_bounce.take().is_some()
        {
            self.send_plugin(PluginCommand::SetRenderMode(
                common::protocol::RenderMode::Realtime,
            ));
            self.restore_engine_song_after_bounce();
            aborted = true;
        }
        aborted
            | self.abort_vocal_synth_export_gate("子プロセスが切断されたため書き出しを中止しました")
    }

    /// 音声 freewheel フェーズ (`AudioRender`) のキャンセル要求。
    ///
    /// 通常は daw_audio プロセスへ IPC で cancel を送り、freewheel ループが次 buffer で
    /// 中断 → `ExportWavComplete { error: None, cancelled: true }` が返る (cancel は
    /// typed flag で伝わる)。標準 WAV export / video 前段のどちらでも有効。
    ///
    /// ただし r.md #75 の **合成完了ゲート**で待っている間は daw_audio がまだ render を
    /// 始めていないので cancel が届かない。その段階ではゲートごと畳んで中止する。
    pub(crate) fn cancel_audio_render(&mut self) {
        if self.abort_vocal_synth_export_gate("書き出しをキャンセルしました") {
            return;
        }
        self.send_audio(common::protocol::AudioCommand::CancelExport);
        self.ui_ephemeral.status_message = "書き出しをキャンセル中...".into();
    }

    /// r.md #75: WAV 書き出しの **合成完了ゲート**で待っている最中なら、それを畳んで
    /// 書き出しごと中止する。畳んだら `true`、待っていなければ `false` (呼び出し側は
    /// 通常の中止手順へ進む)。
    ///
    /// この段階では daw_audio はまだ render を始めていないので、`AudioCommand::CancelExport`
    /// を送っても届かない — daw_audio は次の `ExportWav` 開始時に「前回の残り」として
    /// cancel flag を消すので、**待ちが明けたあと書き出しがそのまま完走してしまう**。
    /// 子プロセス切断 (`handle_child_disconnected`) とユーザーの Cancel/ESC の両方が
    /// ここを通る (待ちを畳む手順は 1 か所)。
    pub(crate) fn abort_vocal_synth_export_gate(&mut self, reason: &str) -> bool {
        if self.ipc.pending_vocal_synth_export.is_empty() {
            return false;
        }
        self.ipc.pending_vocal_synth_export.clear();
        self.transport.pending_export = None;
        // `export_stage` は既に `AudioRender` なので、既存の脱出口がそのまま効く
        // (overlay / 入力 gate / temp WAV / SetRenderMode(Realtime) を畳む)。
        self.abort_audio_export(reason.into());
        true
    }

    /// Phase 7 B4 Step E (2026-05-13): File → Export MIDI...
    /// `rfd` で .mid ファイル保存先を選択 → `midi_export::export_midi`
    /// で SMF1 書き出し。 audio engine への IPC 不要 (= GUI process 単独で
    /// `Song` snapshot を SMF に変換)。 失敗時は status_message に error。
    pub(crate) fn action_export_midi(&mut self) {
        let dialog = rfd::FileDialog::new().add_filter("MIDI", &["mid", "midi"]);
        self.spawn_file_dialog(dialog, FileDialogMode::Save, FileDialogKind::ExportMidi);
    }

    /// File menu → "Export Video..." (`docs/plan_video.md` P8)。
    /// **mp4 出力先を選ぶダイアログ 1 つだけ**。プロジェクト音声は temp WAV へ
    /// 自動レンダリング（daw_audio の freewheel）し、完了（`ExportWavComplete`）
    /// 後に video export して mux する（`action_export_mp4`）。旧仕様の「音声
    /// WAV を別途選ばせる 2 つ目のダイアログ」 は廃止。
    /// `range_beats` はレンジピッカーで確定した書き出し窓 (拍)。
    /// `None` = 全曲。 二重起動ガードはピッカーを開く時点 (`open_export_range_picker`)
    /// で済んでいるが、 ピッカー表示中に状態が変わる経路は無いので念のため残す。
    #[cfg(windows)]
    pub(crate) fn action_open_export_mp4_dialog(
        &mut self,
        range_beats: Option<(f64, f64)>,
        resolution: (u32, u32),
        framerate: f32,
    ) {
        if self.transport.export_stage.is_some()
            || self.transport.pending_video_export.is_some()
            || self.ui_ephemeral.export_dialog_open
        {
            self.ui_ephemeral.status_message = "Video export を実行中です".into();
            return;
        }
        let default_name = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| format!("{s}.mp4"))
            .unwrap_or_else(|| "untitled.mp4".into());
        let dialog = rfd::FileDialog::new()
            .add_filter("MP4 Video", &["mp4"])
            .set_file_name(&default_name)
            .set_title("Export Video to MP4...");
        self.ui_ephemeral.export_dialog_open = true;
        self.ui_ephemeral.status_message = "保存先を選択中...".into();
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::Save,
            FileDialogKind::ExportMp4 { range_beats, resolution, framerate },
        );
    }

    /// `ExportMp4PathChosen` で保存先が確定したときの video export 後段。 旧
    /// `action_open_export_mp4_dialog` が dialog の戻り値で同期に走らせていた
    /// 「音声を temp WAV へ自動レンダリング → 完了後に video export + mux」 を、
    /// dialog 別スレッド化に伴いここへ移設した。 `range_beats` は
    /// 書き出し窓 (拍)。 音声 temp WAV はこの窓に trim して書き、 video render も
    /// 同じ窓で回す (`pending_video_export_range` 経由) ので A/V が揃う。
    pub(crate) fn action_begin_export_mp4(
        &mut self,
        output_path: PathBuf,
        range_beats: Option<(f64, f64)>,
        resolution: (u32, u32),
        framerate: f32,
    ) {
        // audio engine が死んでいる (audio_tx=None) と前段の音声 render が
        // start できず ExportWavComplete が来ない → overlay 永久ロック。
        // 開始前にガードする（標準 WAV export と同じ防御）。
        if self.ipc.audio_tx.is_none() {
            self.ui_ephemeral.status_message =
                "音声エンジンが利用できないため Video export を開始できません".into();
            return;
        }
        let temp_wav = std::env::temp_dir()
            .join(format!("daw01_export_audio_{}.wav", std::process::id()));
        self.transport.pending_video_export = Some(output_path);
        self.transport.pending_video_export_range = range_beats;
        // 音声 render 完了後に始める video render へ、 picker で選んだ
        // 出力解像度 / fps を per-export override として持ち越す。
        self.transport.pending_video_export_dims = Some((resolution, framerate));
        self.transport.export_temp_wav = Some(temp_wav.clone());
        // 前段 = 音声 freewheel。daw_audio の `ExportWavProgress` で determinate
        // 進捗が来る（旧構造では indeterminate「音声レンダリング中」だった）。
        self.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });
        self.transport.export_progress_at = Some(std::time::Instant::now());
        self.ui_ephemeral.status_message = "音声をレンダリング中...".into();
        // 音声も video と同じ窓 (拍) で freewheel render する。 `None` で全曲。
        // stop → reinit plugins → ExportWav (begin_wav_export 経由)。 video render が
        // `.modenv` sidecar を sample して modulation を再現するので、 ここだけ
        // sidecar を書く。
        self.begin_wav_export(temp_wav, range_beats, true);
    }

    /// 音声 freewheel フェーズ (`AudioRender`) を強制終了する。daw_audio が
    /// crash した (`handle_child_disconnected`) / hang して進捗も完了通知も来ない
    /// (`on_tick` watchdog) ときの脱出口。export_stage を None に戻して overlay /
    /// 入力 gate / 再生抑止を解除し、video 前段だった場合は後段に進まず全体中止
    /// (pending_video_export / temp WAV を破棄)、plugin を Realtime へ戻す。
    /// `reason` を status_message に出す。`AudioRender` 中でなければ no-op
    /// (= VideoRender は daw_gui 内なので audio 断の影響を受けない)。
    /// 実際に中止したら `true`、`AudioRender` 中でなく no-op なら `false` を返す。
    /// 呼び出し側 (`handle_child_disconnected`) が status 文言の組み立てに使う。
    pub(crate) fn abort_audio_export(&mut self, reason: String) -> bool {
        if !matches!(self.transport.export_stage, Some(ExportStage::AudioRender { .. })) {
            return false;
        }
        self.transport.export_stage = None;
        self.transport.export_progress_at = None;
        self.transport.pending_video_export = None;
        if let Some(t) = self.transport.export_temp_wav.take() {
            let _ = std::fs::remove_file(&t);
        }
        // daw_audio がまだ生きている (= watchdog が slow render を hang と誤検出した
        // ケース等) 場合、freewheel を止めて export_running を落とさせる。落とさないと
        // CPAL callback が無音を書き続け「再生しても音が出ない」状態になる。crash 時は
        // 既に audio_tx=None なので send_audio は no-op (= 害なし)。
        self.send_audio(AudioCommand::CancelExport);
        // export 開始時に Offline へ切り替えた plugin を Realtime に戻す。plugin
        // host は daw_audio とは別プロセスなので audio 断でも生存している。
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));
        self.ui_ephemeral.status_message = reason;
        true
    }

    /// native file dialog を **別スレッド + owner-modal** で開く共通処理。 dialog を
    /// GUI スレッドで同期に開くと、 dialog 自身のモーダルメッセージポンプが GUI
    /// スレッド上で回り、 preview window 等の 2 枚目 top-level window の WM_PAINT
    /// flood を捌き続けて dialog の入力 (保存ボタン → 上書き確認) が枯れ、 数分
    /// フリーズする (preview window を開いた状態での再現条件)。 構築済み dialog
    /// (`rfd::FileDialog` は `Send`) を専用スレッドへ move し、 main window を
    /// `set_parent` で owner-modal 化して開く。 結果は `FileDialogResult { kind,
    /// paths }` で GUI スレッドへ返し、 `handle_file_dialog_result` が振り分ける。
    pub(crate) fn spawn_file_dialog(
        &self,
        dialog: rfd::FileDialog,
        mode: FileDialogMode,
        kind: FileDialogKind,
    ) {
        #[cfg(windows)]
        let dialog = match self.ui_ephemeral.main_window_hwnd {
            Some(hwnd) => dialog.set_parent(&Win32Parent { hwnd }),
            None => dialog,
        };
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            let paths: Vec<PathBuf> = match mode {
                FileDialogMode::Save => dialog.save_file().into_iter().collect(),
                FileDialogMode::PickFile => dialog.pick_file().into_iter().collect(),
                FileDialogMode::PickFiles => dialog.pick_files().unwrap_or_default(),
            };
            proxy.send(AppEvent::FileDialogResult { kind, paths });
        });
    }

    /// `FileDialogResult` を kind で振り分け、 旧 dialog action の後段ロジックを
    /// GUI スレッドで実行する。 `paths` 空 = キャンセル。
    pub(crate) fn handle_file_dialog_result(&mut self, kind: FileDialogKind, paths: Vec<PathBuf>) {
        match kind {
            FileDialogKind::OpenProject => {
                if let Some(path) = paths.into_iter().next() {
                    self.action_open_path(path);
                }
            }
            FileDialogKind::ExportMp4 { range_beats, resolution, framerate } => {
                // 二重起動ガードを解除し、 Some なら export フロー開始。
                self.ui_ephemeral.export_dialog_open = false;
                match paths.into_iter().next() {
                    #[cfg(windows)]
                    Some(output_path) => {
                        self.action_begin_export_mp4(output_path, range_beats, resolution, framerate)
                    }
                    #[cfg(not(windows))]
                    Some(_output_path) => {
                        let _ = (range_beats, resolution, framerate);
                        self.ui_ephemeral.status_message =
                            "Video export は Windows 専用 (WMF 経由) です".into();
                    }
                    None => {
                        self.ui_ephemeral.status_message =
                            "Video export をキャンセルしました".into();
                    }
                }
            }
            FileDialogKind::ExportWav { range } => {
                // dialog が閉じた（確定 or キャンセル）ので二重起動ガードを解除。
                self.ui_ephemeral.export_dialog_open = false;
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                // audio engine が死んでいる (respawn 失敗 / crash-loop give-up で
                // audio_tx=None) と、ExportWav は send_audio に黙って drop される。
                // ここで export_stage を立ててしまうと完了通知が永遠に来ず overlay
                // + 入力 gate で GUI が永久ロックする。先にガードして start しない。
                if self.ipc.audio_tx.is_none() {
                    self.ui_ephemeral.status_message =
                        "音声エンジンが利用できないため WAV 書き出しを開始できません".into();
                    return;
                }
                self.ui_ephemeral.status_message = "WAV 書き出し中...".to_string();
                // 進捗オーバーレイ（modal）を即表示。最初の `ExportWavProgress` が
                // 来るまでは 0% 表示、以降 daw_audio の freewheel 進捗で更新、
                // `ExportWavComplete` で None に戻して閉じる。これで WAV export 中
                // の入力 gate / 再生抑止も video と同様に効く。
                self.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });
                self.transport.export_progress_at = Some(std::time::Instant::now());
                // standalone WAV export — stop → reinit plugins →
                // (on PluginsReinitDone) ExportWav。begin_wav_export が再生停止 /
                // LoadSong / SetRenderMode(Offline) / 全 plugin 再初期化を行う。
                // modulation は音に焼き込み済みなので `.modenv` sidecar は書かない。
                self.begin_wav_export(path, range, false);
            }
            FileDialogKind::ExportMidi => {
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                match crate::midi_export::export_midi(self.song_doc.song(), &path) {
                    Ok(()) => {
                        self.ui_ephemeral.status_message =
                            format!("MIDI 書き出し完了: {}", path.display());
                    }
                    Err(e) => {
                        self.ui_ephemeral.status_message = format!("MIDI 書き出し失敗: {e}");
                        tracing::error!(error = %e, path = %path.display(), "MIDI export failed");
                    }
                }
            }
            FileDialogKind::ImportAudio => {
                if !paths.is_empty() {
                    // dialog 経由は位置情報がないので NoHint (cursor track / playhead)。
                    self.action_import_audio(paths, ImportTrackTarget::NoHint, None);
                }
            }
            FileDialogKind::ImportVideo => {
                if !paths.is_empty() {
                    // dialog 経由は位置情報がないので NoHint (= 一番下に video + audio)。
                    self.action_import_video(paths, ImportTrackTarget::NoHint, None);
                }
            }
            FileDialogKind::ImportImage => {
                if !paths.is_empty() {
                    // dialog 経由は位置情報がないので NoHint (= 一番下に新規 track)。
                    self.action_import_image(paths, ImportTrackTarget::NoHint, None);
                }
            }
            FileDialogKind::ImportMidi => {
                if !paths.is_empty() {
                    // dialog 経由は位置情報がないので NoHint (= 一番下に新規 track、
                    // 開始位置は playhead)。
                    self.action_import_midi(paths, ImportTrackTarget::NoHint, None);
                }
            }
            FileDialogKind::AddAudioEvent {
                clip,
                position_in_clip_beats,
            } => {
                if let Some(path) = paths.into_iter().next() {
                    self.handle_event(AppEvent::AddAudioEventFromFile {
                        clip,
                        path,
                        position_in_clip_beats,
                    });
                }
            }
        }
    }

    /// Synchronous mp4 render (`docs/plan_video.md` P8). Blocks the
    /// GUI thread for the duration — typical 1-minute MV at 1080p30
    /// finishes in ~10s on a recent laptop (CPU NV12 conversion is
    /// the bottleneck). Surface progress / completion via
    /// `status_message`; failure surfaces the error there too.
    #[cfg(windows)]
    /// mp4 export を **background thread** で実行する。長尺 / 多レイヤーの
    /// project は 1 フレーム ~100ms（GPU readback + 動画デコード）で数十秒〜
    /// 数分かかるため、 GUI スレッド同期だと UI とファイルダイアログが固まる
    /// （= 旧挙動でハングと誤認されていた）。進捗は `ExportProgress`、完了は
    /// `ExportFinished` を `event_proxy` 経由で送り、 UI が進捗オーバーレイ +
    /// Cancel を出す。
    pub(crate) fn action_export_mp4(
        &mut self,
        output_path: PathBuf,
        audio_wav: Option<PathBuf>,
        range_beats: Option<(f64, f64)>,
        // picker で選んだ出力解像度 / fps の per-export override。
        // `None` ならプロジェクト値 (`Song.video_resolution` / `video_framerate`) を使う。
        dims: Option<((u32, u32), f32)>,
    ) {
        // 何らかの export が走っている間は再入を弾く。video 後段への chain は
        // `ExportWavComplete` ハンドラが先に `export_stage` を None に戻してから
        // 呼ぶので通る。
        if self.transport.export_stage.is_some() {
            self.ui_ephemeral.status_message = "Video export を実行中です".into();
            return;
        }
        let project_dir = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let song = self.song_doc.song().clone();
        let proxy = self.ipc.event_proxy.clone();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.transport.export_cancel = Some(cancel.clone());
        self.transport.export_stage = Some(ExportStage::VideoRender { done: 0, total: 0 });
        self.ui_ephemeral.status_message = format!("Video export 開始: {}", output_path.display());
        std::thread::spawn(move || {
            // video の render 窓も拍範囲に合わせる (audio temp WAV は
            // 既に同じ窓に trim 済み → frame 0 で A/V が揃う)。
            // per-export override を分解 (None ならビルダーに None を渡し、
            // RenderConfig が resolved_* で song 値へフォールバックする)。
            let (override_res, override_fps) = match dims {
                Some((res, fps)) => (Some(res), Some(fps)),
                None => (None, None),
            };
            let cfg = crate::render_video::RenderConfig::new(&song, &output_path)
                .with_project_dir(project_dir.as_deref())
                .with_audio_wav(audio_wav.as_deref())
                .with_range_beats(range_beats)
                .with_output_resolution(override_res)
                .with_output_framerate(override_fps);
            // 進捗は 5 フレームごと（+ 開始 / 完了）に間引いて送る（毎フレーム
            // 送ると event queue を圧迫する）。
            let mut last_sent = 0u64;
            let mut on_progress = |done: u64, total: u64| {
                if done == 0 || done >= total || done.saturating_sub(last_sent) >= 5 {
                    last_sent = done;
                    proxy.send(AppEvent::ExportProgress { done, total });
                }
            };
            let result = crate::render_video::render_mp4_cancellable(
                &cfg,
                &cancel,
                &mut on_progress,
            )
            .map(|stats| stats.output_path);
            proxy.send(AppEvent::ExportFinished { result });
        });
    }

}
