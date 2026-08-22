//! r.md #61: アプリ終了シーケンスの状態機械 (SSoT)。
//!
//! # なぜ要るか
//!
//! 旧実装の終了は「`ui_ephemeral.should_quit` を立てる → 同じ frame で
//! `event_loop.exit()` → `drop(bootstrap)` → `JobHandle` の最後の `Arc` が
//! 落ちて `CloseHandle` → Job Object が子プロセスを **TerminateProcess**」
//! だった。つまり **強制 kill が例外経路ではなく正規経路**で、
//!
//! - CLAP の `gui.destroy` / `deactivate` / `destroy` / `entry.deinit`
//! - VST3 の `IPlugView::removed` / `setProcessing(0)` / `setActive(0)` / `terminate()`
//! - WASAPI デバイスの解放 (`cpal::Stream::drop`)
//!
//! が一度も走らなかった。実測ログでは "daw_plugin_host shutting down" 612 回に
//! 対し "daw_plugin_host exiting" は **1 回**、プラグイン 0 個のセッションですら
//! 91% がレースに負けていた (kill window はサブミリ秒)。
//!
//! # 理想形
//!
//! **すべての終了経路が 1 つのシーケンスに合流し、子プロセスは自分で正しく死ぬ。**
//! Job Object は crash / hang 時の backstop に格下げする (撤去はしない —
//! `std::mem::forget` された VOICEVOX engine の最終的な保険でもある)。
//!
//! 入口 (全部 [`crate::event::AppEvent::Quit`] に合流する):
//! - `WindowEvent::CloseRequested` (✕ / Alt+F4 / システムメニュー / タスクバー)
//! - File > 終了、`Ctrl+Q`
//! - `--smoke-test` オーケストレータ (終了コード付き)
//! - Windows のサインアウト / シャットダウン (`crate::session_end`)
//!
//! 完了判定は **子プロセスの exit そのもの**を真実源にする。返信 event ではない
//! —「返事を書けた」と「DLL を unload し終えた」は別の事実で、欲しい保証は後者
//! だけだから。待ちは [`DRAIN_TIMEOUT`] で有界 (アーキテクチャ不変条件 4)。

use std::time::{Duration, Instant};

use common::protocol::ChildKind;

/// 子プロセスの graceful teardown を待つ上限。超えたら backstop (Job Object)
/// に委ねて先へ進む。**無限待ちにしない** — 応答しないプラグインの
/// `deactivate` / `FreeLibrary` でアプリの終了が固まってはいけない。
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 終了要求。全終了経路がこの 1 つの型に合流する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuitRequest {
    /// 未保存確認ダイアログを飛ばす。ユーザー操作では **必ず false**。
    /// `true` にしてよいのは、人間の判断を仰げない自動実行経路
    /// (`--smoke-test`) だけ。
    pub skip_dirty_guard: bool,
    /// プロセスの終了コード。GUI からの終了は常に 0、smoke test は
    /// 判定結果を載せる。
    pub exit_code: u8,
}

impl QuitRequest {
    /// ユーザー操作による終了 (✕ / Alt+F4 / File > 終了 / Ctrl+Q /
    /// Windows セッション終了)。未保存なら確認する。
    pub const USER: Self = Self {
        skip_dirty_guard: false,
        exit_code: 0,
    };

    /// 自動実行 (smoke test) の終了。確認ダイアログを出す相手が居ないので
    /// ガードを飛ばし、判定結果を終了コードで返す。
    #[must_use]
    pub const fn automated(exit_code: u8) -> Self {
        Self {
            skip_dirty_guard: true,
            exit_code,
        }
    }
}

/// 終了シーケンスの段階。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShutdownPhase {
    /// 通常運転。
    #[default]
    Running,
    /// 子プロセスへ終了を伝え、exit を待っている。UI は「終了処理中…」の
    /// モーダルだけを見せ、編集入力は受け付けない。
    Draining,
    /// 全子プロセスが exit した (or 期限超過)。`event_loop.exit()` してよい。
    Finished,
}

/// 終了シーケンスの状態 (`AppData.shutdown`)。
///
/// 「終了を決めた」(旧 `should_quit`) と「終了処理中」を分けるのが要点。
/// 旧 `should_quit` は立った瞬間に `exit()` まで走り切る一瞬の値だったので、
/// 子の teardown を待つ段を表現できなかった。
#[derive(Debug, Default)]
pub struct ShutdownState {
    phase: ShutdownPhase,
    /// `Draining` に入った時刻 (経過表示用)。
    started_at: Option<Instant>,
    /// `Draining` を打ち切る時刻。runner はこれを `ControlFlow::WaitUntil` に
    /// 使うので、イベントが来なくても期限で必ず起きる。
    deadline: Option<Instant>,
    exit_code: u8,
    /// 期限超過で backstop に落ちたか。
    forced: bool,
    /// 期限超過時にまだ生きていた子 (ログ / 次回起動の status 用)。
    stragglers: Vec<ChildKind>,
}

impl ShutdownState {
    #[must_use]
    pub fn phase(&self) -> ShutdownPhase {
        self.phase
    }

    /// 終了シーケンスに入っている (= 進行中 or 完了)。respawn 抑止 /
    /// 入力ゲートの判定はこれ。
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        !matches!(self.phase, ShutdownPhase::Running)
    }

    /// 子プロセスの exit を待っている最中。
    #[must_use]
    pub fn is_draining(&self) -> bool {
        matches!(self.phase, ShutdownPhase::Draining)
    }

    /// `event_loop.exit()` してよい。
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self.phase, ShutdownPhase::Finished)
    }

    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    #[must_use]
    pub fn forced(&self) -> bool {
        self.forced
    }

    #[must_use]
    pub fn stragglers(&self) -> &[ChildKind] {
        &self.stragglers
    }

    /// `Draining` に入ってからの経過。「終了処理中…」の表示に使う。
    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.started_at
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
    }

    /// 子の teardown 待ちを開始する。既に開始済みなら `false` (= 多重起動の
    /// 抑止。✕ 連打 / メニューとショートカットの二重発火で 2 度走らせない)。
    pub fn begin(&mut self, exit_code: u8, now: Instant) -> bool {
        if self.is_shutting_down() {
            return false;
        }
        self.phase = ShutdownPhase::Draining;
        self.started_at = Some(now);
        self.deadline = Some(now + DRAIN_TIMEOUT);
        self.exit_code = exit_code;
        true
    }

    /// teardown が終わった (`stragglers` が空でなければ期限超過)。
    pub fn finish(&mut self, stragglers: Vec<ChildKind>) {
        if matches!(self.phase, ShutdownPhase::Finished) {
            return;
        }
        self.forced = !stragglers.is_empty();
        self.stragglers = stragglers;
        self.phase = ShutdownPhase::Finished;
        self.deadline = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_は一度だけ受理して多重起動を弾く() {
        let mut s = ShutdownState::default();
        let now = Instant::now();
        assert!(s.begin(0, now));
        assert!(s.is_draining());
        // ✕ 連打 / メニュー + ショートカットの二重発火。
        assert!(!s.begin(3, now));
        assert_eq!(s.exit_code(), 0, "後から来た終了コードで上書きしない");
    }

    #[test]
    fn finish_は残った子を記録して強制終了扱いにする() {
        let mut s = ShutdownState::default();
        s.begin(0, Instant::now());
        s.finish(vec![ChildKind::PluginHost]);
        assert!(s.is_finished());
        assert!(s.forced());
        assert_eq!(s.stragglers(), &[ChildKind::PluginHost]);
        // 全員 exit したケースは forced にならない。
        let mut clean = ShutdownState::default();
        clean.begin(0, Instant::now());
        clean.finish(Vec::new());
        assert!(!clean.forced());
    }
}
