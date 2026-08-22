//! 範囲ラウドネス解析の進行状態とレポート (r.md #54)。
//!
//! 「どの範囲を測っているか」「走査中か」「最後の結果」を持つ session state。
//! **プロジェクトには保存しない** — ラウドネスは全トラック・全プラグイン・
//! 全オートメーション・マスターチェーンの合成結果で、プラグイン内部状態
//! (VCV Rack のパッチ / ARA の編集 / VOICEVOX の合成キャッシュ) まで含む。
//! これを fingerprint に畳むのは原理的に不可能なので、**キャッシュせず毎回測り、
//! Song が編集されたら「古い」と明示する** ([`LoudnessState::report_epoch`] と
//! `SongDoc::edit_epoch` の比較 = `AppData::loudness_report_stale`)。
//! epoch は編集だけでなく undo / redo / プロジェクト差し替えでも進むので、
//! 「編集の口ごとに印を立てる」より穴が開かない。

use common::loudness_report::LoudnessReport;

/// 解析の進行段階。
#[derive(Debug, Clone, PartialEq)]
pub enum LoudnessPhase {
    /// 解析していない。
    Idle,
    /// 全プラグインの再初期化 (deactivate→activate) 完了待ち。
    /// `PluginEvent::PluginsReinitDone` で `AudioCommand::AnalyzeLoudness` を撃つ。
    /// `range` は拍 (`None` = 全曲)。
    AwaitingReinit { range: Option<(f64, f64)> },
    /// daw_audio が freewheel 走査中。
    Running,
    /// 中止を要求済み (完了通知待ち)。
    Cancelling,
}

impl LoudnessPhase {
    /// 走査に関わっている (= 再生 / 編集を止めて背景を暗転する) か。
    #[must_use]
    pub fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// 解析セッションの状態 (session-only、プロジェクトに保存しない)。
#[derive(Debug)]
pub struct LoudnessState {
    pub phase: LoudnessPhase,
    /// 解析セッションの世代。開始のたびに増やし、`AudioEvent` を受けたときに
    /// **一致するものだけ**採用する。中止 / watchdog のあとに前回の走査から
    /// 後着した完了通知を、新しいセッションの結果として受理してしまうのを防ぐ
    /// (受理すると新しい解析が永久に発火しない)。
    pub generation: u64,
    /// `generation` のうち、engine が実際に走らせている (はずの) 世代。
    /// 後着イベントの判定に使う。
    pub running_generation: u64,
    /// 走査中は途中経過、完了後は確定値。`None` = まだ一度も測っていない。
    pub report: Option<Box<LoudnessReport>>,
    /// レポートを取った時点の `SongDoc::edit_epoch`。現在値と違えば「古い」。
    /// `edit_song` 経由の編集だけでなく **undo / redo / プロジェクト差し替え**も
    /// epoch を進めるので、これ 1 つで全経路を捉えられる。
    pub report_epoch: u64,
    /// 直近の失敗理由 (レポート窓に出す)。成功でクリアする。
    pub error: Option<String>,
    /// 進捗が最後に動いた時刻。daw_audio が hang して完了通知も進捗も来ない
    /// 状態を `on_tick` の watchdog が検出して窓を解放するために使う
    /// (書き出しの `export_progress_at` と同じ役割)。
    pub progress_at: Option<std::time::Instant>,
}

impl Default for LoudnessState {
    fn default() -> Self {
        Self {
            phase: LoudnessPhase::Idle,
            generation: 0,
            running_generation: 0,
            report: None,
            report_epoch: 0,
            error: None,
            progress_at: None,
        }
    }
}
