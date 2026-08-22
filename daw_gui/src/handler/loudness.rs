//! handler::loudness — 範囲ラウドネス解析の起動・進捗・完了 (r.md #54)。
//!
//! 解析は **WAV 書き出しとまったく同じ手順**で走る: 再生停止 → 最新 song を
//! engine へ flush → `SetRenderMode(Offline)` → 全プラグインを
//! deactivate/activate で初期化 → `PluginsReinitDone` を待って
//! `AudioCommand::AnalyzeLoudness`。再初期化を省くとプラグイン内部状態が直前の
//! 再生から引き継がれ、**同じ範囲を測るたびに値が変わる**ので省けない。
//!
//! 走査そのものは daw_audio の freewheel (`export::run_loudness_analysis`) で、
//! `render_master_buffer` の出力をライブメーターと同一の測定器
//! (`common::loudness`) に流す。よって「解析値」と「その範囲を書き出した WAV の
//! 値」は構造的に一致する。

use crate::app_types::*;
use crate::state::*;
use common::loudness_report::LoudnessReport;
use common::protocol::{AudioCommand, PluginCommand};

/// 進捗も完了も来ないまま解析が固まったとみなす時間。書き出しの watchdog
/// (`handler::tick`) と同じ 60 秒。
pub(crate) const LOUDNESS_WATCHDOG_SECS: u64 = 60;

impl AppData {
    /// 解析 → 「ラウドネス解析...」 / ルーラー右クリック / マスターパネルの
    /// 「解析」 / `Ctrl+L`。 範囲ピッカーを開く (既定 = ループ範囲)。
    pub(crate) fn open_loudness_range_picker(&mut self) {
        if self.loudness.phase.is_busy() {
            self.ui_ephemeral.status_message = "ラウドネス解析を実行中です".into();
            return;
        }
        if self.transport.export_stage.is_some() {
            self.ui_ephemeral.status_message = "書き出し中はラウドネス解析を開始できません".into();
            return;
        }
        self.open_export_range_picker(ExportRangeKind::Loudness);
    }

    /// 範囲ピッカー確定 → 解析開始。`range` は拍 (`None` = 全曲)。
    ///
    /// レポート窓を**先に**開いて、その中に進捗と中止ボタンを出す
    /// (grill-me 2026-08-16 で確定: 解析中は背景を暗転して操作を遮断し、
    /// 完了したら暗転が消えて窓だけ残る)。
    pub(crate) fn begin_loudness_analysis(&mut self, range: Option<(f64, f64)>) {
        // 走査は engine の `export_running` を書き出しと共有する。二重起動は
        // engine 側でも弾かれるが、GUI 側でも状態を壊さないようここで止める
        // (「測り直す」など、ピッカーを経由しない経路の唯一の防波堤)。
        if self.offline_render_busy() {
            self.ui_ephemeral.status_message =
                "オフライン処理の実行中は解析を開始できません".into();
            return;
        }
        if self.ipc.audio_tx.is_none() {
            self.ui_ephemeral.status_message =
                "音声エンジンが利用できないためラウドネス解析を開始できません".into();
            return;
        }
        // 空範囲は測るものが無い (engine 側でも 0 フレームになる)。
        if let Some((s, e)) = range
            && e - s <= f64::EPSILON
        {
            self.ui_ephemeral.status_message = "解析する範囲が空です".into();
            return;
        }
        self.ui_prefs.loudness_report_open = true;
        self.persist_app_config();
        self.loudness.error = None;
        // 前回の結果は残さない (新しい範囲の途中経過と混ざって「どの範囲の値か」
        // が分からなくなる)。
        self.loudness.report = None;

        // **先に停止してから編集を止める**。stop() は録音セッションのクローズ
        // (押しっぱなしノートの長さ確定 = Song 編集) を含むので、順序を逆にすると
        // その編集がロックに弾かれてノートが伸びたままになる。
        self.stop();

        self.loudness.generation = self.loudness.generation.wrapping_add(1);
        self.loudness.phase = LoudnessPhase::AwaitingReinit { range };
        self.loudness.progress_at = Some(std::time::Instant::now());
        // 走査中の編集を止める。`edit_song` の入口同期だけだと、`song_doc.edit` を
        // 直接呼ぶ経路 (BPM スクラブ等) が始まった直後に素通りする。
        self.sync_export_lock();

        // 書き出しと同じ前処理。
        self.flush_song_sync();
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_plugin(PluginCommand::ReinitAllPlugins);
        self.ui_ephemeral.status_message = "ラウドネスを解析中...".into();
    }

    /// `PluginsReinitDone` を受けて実際の走査を発火する。
    /// 待っていなければ `false` (書き出し側の pending と共存するため)。
    pub(crate) fn fire_pending_loudness_analysis(&mut self) -> bool {
        let LoudnessPhase::AwaitingReinit { range } = self.loudness.phase else {
            return false;
        };
        self.loudness.phase = LoudnessPhase::Running;
        self.loudness.running_generation = self.loudness.generation;
        self.loudness.progress_at = Some(std::time::Instant::now());
        self.send_audio(AudioCommand::AnalyzeLoudness { range });
        true
    }

    /// 中止 (レポート窓の「中止」ボタン / 解析中の Esc)。
    pub(crate) fn cancel_loudness_analysis(&mut self) {
        match self.loudness.phase {
            // まだ engine へ投げていない = その場で畳む。
            LoudnessPhase::AwaitingReinit { .. } => {
                self.abort_loudness_analysis("ラウドネス解析をキャンセルしました".into());
            }
            LoudnessPhase::Running => {
                self.loudness.phase = LoudnessPhase::Cancelling;
                self.send_audio(AudioCommand::CancelExport);
            }
            LoudnessPhase::Cancelling | LoudnessPhase::Idle => {}
        }
    }

    /// 解析を **GUI 側から強制終了** する (watchdog / 子プロセス切断 / 再初期化待ちの
    /// 中止)。engine 側が走っている可能性があるので **必ず `CancelExport` を送る**。
    ///
    /// 送らないと `EngineShared::export_running` が立ちっぱなしになり、CPAL
    /// コールバックが無音を書き続けて「再生しても音が出ない」状態に陥り、以後の
    /// 書き出し / バウンス / 解析も全部 "export already in progress" で弾かれる
    /// (書き出し側 `abort_audio_export` が同じ理由で送っている)。
    pub(crate) fn abort_loudness_analysis(&mut self, reason: String) {
        if !self.loudness.phase.is_busy() {
            return;
        }
        self.loudness.phase = LoudnessPhase::Idle;
        self.loudness.progress_at = None;
        // 世代を進めて、後から届く前セッションの進捗 / 完了を捨てる。
        self.loudness.generation = self.loudness.generation.wrapping_add(1);
        self.loudness.report = None;
        self.send_audio(AudioCommand::CancelExport);
        self.finish_loudness_render_mode();
        self.ui_ephemeral.status_message = reason;
    }

    /// 走査中の途中経過。進捗バーと、伸びていくグラフの両方をこれで更新する。
    pub(crate) fn on_loudness_progress(&mut self, report: Box<LoudnessReport>) {
        if !self.is_current_loudness_session() {
            // 中止 / watchdog 後に届いた後着。表示中の確定値を壊さない。
            return;
        }
        self.loudness.progress_at = Some(std::time::Instant::now());
        self.loudness.report = Some(report);
    }

    /// 解析完了 / 中止 / 失敗。
    pub(crate) fn on_loudness_complete(
        &mut self,
        report: Option<Box<LoudnessReport>>,
        error: Option<String>,
        cancelled: bool,
    ) {
        if !self.is_current_loudness_session() {
            tracing::warn!(
                ?error,
                cancelled,
                "LoudnessAnalysisComplete for a stale session; ignoring"
            );
            return;
        }
        self.loudness.phase = LoudnessPhase::Idle;
        self.loudness.progress_at = None;
        self.finish_loudness_render_mode();
        if let Some(err) = error {
            self.loudness.error = Some(err.clone());
            self.loudness.report = None;
            self.ui_ephemeral.status_message = format!("ラウドネス解析に失敗: {err}");
            return;
        }
        // 中止でも「そこまで測った値」は返るが、範囲全体の Integrated ではない
        // ので確定値として残さない。
        if cancelled {
            self.loudness.report = None;
            self.ui_ephemeral.status_message = "ラウドネス解析をキャンセルしました".into();
            return;
        }
        if let Some(r) = report {
            let msg = if r.integrated_lufs.is_finite() {
                format!(
                    "ラウドネス解析完了: {:.1} LUFS / LRA {:.1} LU / TP {:.1} dBTP",
                    r.integrated_lufs, r.lra_lu, r.true_peak_dbtp
                )
            } else {
                "ラウドネス解析完了: 範囲がほぼ無音です".to_string()
            };
            self.loudness.report = Some(r);
            // 「この値がどの Song 状態のものか」を epoch で固定する。以後 epoch が
            // 進めば (編集 / undo / redo / プロジェクト差し替え) 自動的に古くなる。
            self.loudness.report_epoch = self.song_doc.edit_epoch();
            self.ui_ephemeral.status_message = msg;
        }
    }

    /// 届いた解析イベントが「今のセッションのもの」か。
    fn is_current_loudness_session(&self) -> bool {
        self.loudness.phase.is_busy()
            && self.loudness.running_generation == self.loudness.generation
    }

    /// レポートの値が現在の Song に対して古いか (= 測ってから編集された)。
    #[must_use]
    pub fn loudness_report_stale(&self) -> bool {
        self.loudness.report.is_some() && self.loudness.report_epoch != self.song_doc.edit_epoch()
    }

    /// 解析の後始末: 編集ロックを解いてプラグインを Realtime へ戻す。書き出しが
    /// 同時に走っていることはない (engine 側が `export_running` で排他している)
    /// ので無条件でよい。
    fn finish_loudness_render_mode(&mut self) {
        self.sync_export_lock();
        self.send_plugin(PluginCommand::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));
    }

    /// レポート窓の開閉 (解析メニュー / `Ctrl+L` / ✕ / Esc)。
    pub(crate) fn toggle_loudness_report(&mut self) {
        self.ui_prefs.loudness_report_open = !self.ui_prefs.loudness_report_open;
        self.persist_app_config();
    }

    /// 目標ラウドネスとトゥルーピーク上限を同時に差し替える (配信プリセット)。
    /// マスターパネルのラウドネスメーターの 0 LU 線もこれで動く (基準は 1 つ)。
    pub(crate) fn set_loudness_target(&mut self, target_lufs: f32, ceiling_dbtp: f32) {
        self.ui_prefs.meter_settings.loudness_target_lufs = target_lufs;
        self.ui_prefs.meter_settings.loudness_true_peak_ceiling_dbtp = ceiling_dbtp;
        self.sync_meter_control();
        self.persist_app_config();
    }

    /// レポート内の秒位置へプレイヘッドを飛ばす (最大値の行 / グラフのクリック)。
    pub(crate) fn seek_to_loudness_position(&mut self, secs: f32) {
        let Some(report) = self.loudness.report.as_ref() else {
            return;
        };
        let frame = report.song_frame_at(secs);
        let beat =
            common::automation::samples_to_beats(self.song_doc.song(), report.sample_rate, frame);
        self.seek_playhead_to(beat);
    }
}
