//! handler::shutdown — r.md #61: 終了シーケンスの実行 (`AppData` 側)。
//!
//! 状態機械そのものは [`crate::shutdown`]。ここはその状態遷移に伴う
//! **副作用** (子プロセスへのコマンド送出 / VOICEVOX engine 停止 /
//! recovery ファイル削除 / 完了判定) を持つ。
//!
//! 全終了経路 (✕ / Alt+F4 / File > 終了 / Ctrl+Q / smoke test /
//! Windows セッション終了) は [`AppData::request_quit`] に合流する。

use std::time::Instant;

use common::protocol::{AudioCommand, ChildKind, PluginCommand};

use crate::app_types::DirtyGuardAction;
use crate::shutdown::QuitRequest;
use crate::state::AppData;

impl AppData {
    /// **全終了経路の唯一の入口**。未保存なら確認モーダルを挟み、通れば
    /// [`Self::begin_shutdown`] へ進む。
    ///
    /// ガード (dirty 確認 / `pending_state_queue` 待ち) は New / Open と共通の
    /// `request_guarded_action` をそのまま使う。
    ///
    /// **終了は New / Open より上位の意図**なので、それらのガードが保留中でも
    /// 置き換えて通す。置き換えないと `request_guarded_action` の再入ガード
    /// (`dirty_guard.is_some()` 等) に黙って飲まれ、「OS のシャットダウンには
    /// 『未保存だから待って』と答えたのに、こちらは終了を始めていない」という
    /// 噛み合わない状態になる (New の確認モーダルを開いたまま席を立った場合)。
    pub fn request_quit(&mut self, req: QuitRequest) {
        if self.shutdown.is_shutting_down() {
            return; // ✕ 連打 / メニューとショートカットの二重発火。
        }
        if req.skip_dirty_guard {
            // 人間の判断を仰げない自動実行 (smoke test)。確認モーダルを出す
            // 相手が居ないので、保留中のガード状態ごと捨てて直行する。
            self.clear_pending_guards();
            self.begin_shutdown(req);
            return;
        }
        // 既に「終了」を聞いている最中なら何もしない (モーダルを開き直さない)。
        if matches!(
            self.ui_ephemeral.dirty_guard,
            Some(DirtyGuardAction::Quit(_))
        ) {
            return;
        }
        // 保留中の New / Open は終了で置き換える。round-trip 待ちの保存
        // (`pending_state_queue`) はそのまま残るので、drain 後に
        // `request_guarded_action` が終了で再評価する。
        self.clear_pending_guards();
        self.request_guarded_action(DirtyGuardAction::Quit(req));
    }

    /// 保留中のガード意図を全部捨てる (置き換えの前段)。
    fn clear_pending_guards(&mut self) {
        self.ui_ephemeral.dirty_guard = None;
        self.ui_ephemeral.guard_after_save = None;
        self.ui_ephemeral.guard_pending_action = None;
    }

    /// 終了シーケンスを開始する。ここから先はユーザー入力を受け付けず、
    /// 「終了処理中…」だけを見せて子プロセスの exit を待つ。
    ///
    /// 順序が意味を持つ:
    /// 1. 走行中のオフライン処理 (書き出し / ラウドネス解析) を中止する —
    ///    freewheel の最中に `Shutdown` を投げると、engine は render thread を
    ///    抱えたまま recv_loop を抜けることになる。
    /// 2. transport を止める — プラグインの `process()` を回している最中に
    ///    deactivate させない。
    /// 3. `PluginCommand::Shutdown` — 全 device を `teardown_device`
    ///    (stop_processing → deactivate → gui_destroy → drop) で畳ませ、
    ///    worker pool を止めて exit させる。
    /// 4. `AudioCommand::Shutdown` — CPAL stream を pause + drop させて exit。
    /// 5. VOICEVOX engine を止める (spawn したのが我々の場合だけ)。
    /// 6. 開いている picker / help を畳む — drain 中も event loop は回るので、
    ///    残しておくと「終了処理中…」の下に居座って見た目が壊れる。
    ///
    /// recovery ファイルの削除 (`on_shutdown`) は **ここではやらない**。
    /// 本当に終わるとき (`poll_shutdown` の `finish`) に寄せる — drain の途中で
    /// OS に強制終了された場合、まだ終われていないのだから復旧候補は残るのが
    /// 正しい。
    ///
    /// 3 と 4 の順序: plugin_host は worker pool 越しに daw_audio と対で
    /// 動いているが、どちらの `Shutdown` ハンドラも相手の生死に依存しない
    /// (pool の停止は各プロセスが自分の側だけを畳む) ので、先に重い方
    /// (プラグインの unload) を投げて並行に進めさせる。
    pub fn begin_shutdown(&mut self, req: QuitRequest) {
        if !self.shutdown.begin(req.exit_code, Instant::now()) {
            return;
        }
        tracing::info!(exit_code = req.exit_code, "shutdown sequence started");

        // (0) pipe loop に「これ以降の切断は crash ではない」と伝える。
        //     子が自力 exit すると pipe は子側から先に閉じるので、これが無いと
        //     reader が EOF を拾って `ChildDisconnected` を合成し、respawn が
        //     走って「終了しようとしているのに子が生き返る」。
        if let Some(sup) = self.ipc.supervisor.as_ref() {
            sup.begin_shutdown();
        }

        // (1) オフライン処理を中止。engine 側の render thread に「もう要らない」
        //     を伝えてから終了要求を出す。
        if self.transport.export_stage.is_some() || self.loudness.phase.is_busy() {
            self.send_audio(AudioCommand::CancelExport);
        }

        // (2) transport 停止。
        self.send_audio(AudioCommand::Stop);
        self.transport.is_playing = false;

        // (3)(4) 子プロセスへ終了要求。
        self.send_plugin(PluginCommand::Shutdown);
        self.send_audio(AudioCommand::Shutdown);

        // (5) VOICEVOX engine (我々が spawn したものだけ)。
        self.stop_spawned_voicevox_engine();

        // (6) 開いている picker / help を畳む。以後 `handle_event` は全 event を
        //     捨てるので操作は届かないが、描かれたままだと「終了処理中…」の下に
        //     残って見た目が壊れる (暗幕も二重になる)。
        self.close_transient_ui_for_shutdown();
    }

    /// 終了に入るときに畳む一時 UI。ここに挙げ忘れても **数秒だけ古いパネルが
    /// 見えるだけ** (操作は `handle_event` の gate が全部落とす) なので、
    /// 「列挙漏れ = 永久ロック」の失敗モードにはならない。
    fn close_transient_ui_for_shutdown(&mut self) {
        self.ui_ephemeral.is_plugin_picker_open = false;
        self.ui_ephemeral.is_font_picker_open = false;
        self.ui_ephemeral.font_picker_target = None;
        self.ui_ephemeral.send_picker = None;
        self.ui_ephemeral.export_range_picker = None;
        self.ui_ephemeral.show_recovery_modal = false;
        self.ui_prefs.is_help_open = false;
    }

    /// 子プロセスの exit を観測し、全員終わっていれば (or 期限を過ぎていれば)
    /// 終了シーケンスを完了させる。runner が frame ごと + deadline 到達で呼ぶ。
    ///
    /// 完了判定に返信 event を使わないのは、「返事を書けた」と「DLL を unload
    /// し終えた」が別の事実だから。欲しい保証は後者だけで、それを外から
    /// 確かめられるのはプロセスの exit しかない。
    pub fn poll_shutdown(&mut self) {
        if !self.shutdown.is_draining() {
            return;
        }
        let Some(sup) = self.ipc.supervisor.clone() else {
            // supervisor 無し (script / test 経路) は待つ相手が居ない。
            self.finish_shutdown(Vec::new());
            return;
        };
        let live = sup.poll_live_children();
        if live.is_empty() {
            tracing::info!(
                elapsed_ms = self.shutdown.elapsed(Instant::now()).as_millis() as u64,
                "all child processes exited cleanly"
            );
            self.finish_shutdown(Vec::new());
            return;
        }
        let now = Instant::now();
        if self.shutdown.deadline().is_some_and(|d| now >= d) {
            let names: Vec<&str> = live.iter().map(|k| k.as_str()).collect();
            tracing::warn!(
                stragglers = ?names,
                timeout_ms = crate::shutdown::DRAIN_TIMEOUT.as_millis() as u64,
                "child processes did not exit in time; falling back to the job-object backstop"
            );
            self.finish_shutdown(live);
        }
    }

    /// `Draining` → `Finished`。**recovery ファイルの削除はここ**。
    ///
    /// drain の開始時に消すと、teardown に数秒かかる間に autosave が書き直したり
    /// (event gate で塞いだが)、OS に強制終了されて「まだ終われていないのに
    /// 復旧候補だけ消えている」状態になりうる。「本当に終わる瞬間に消す」が正しい。
    fn finish_shutdown(&mut self, stragglers: Vec<ChildKind>) {
        self.shutdown.finish(stragglers);
        self.on_shutdown();
    }

    /// 「終了処理中…」モーダルに出す 1 行。
    ///
    /// 経過秒を出すのは **止まっていないことを見せる**ため。どの子が残って
    /// いるかはログ側 (`poll_shutdown` の warn) に出す — 画面に
    /// 「daw_plugin_host」と書いても曲を作っている人には何の助けにもならない。
    #[must_use]
    pub fn shutdown_status_line(&self, now: Instant) -> String {
        let secs = self.shutdown.elapsed(now).as_secs();
        if secs >= 1 {
            format!("プラグインを解放しています… ({secs} 秒)")
        } else {
            "プラグインを解放しています…".to_string()
        }
    }

    /// 期限超過で backstop (Job Object の強制 kill) に落ちたときの説明。
    ///
    /// 出し先は **ログだけ** — この時点で画面はもう畳まれる直前で、読める時間が
    /// 無い。次回起動時に見せるには「前回の終了が強制だった」を永続化する必要が
    /// あるが、それは終了の失敗という稀な事象のために保存状態を 1 つ増やすことに
    /// なるので採らない。調べに行く先は `%LOCALAPPDATA%\daw_01\logs`。
    #[must_use]
    pub fn shutdown_forced_message(&self) -> Option<String> {
        if !self.shutdown.forced() {
            return None;
        }
        let names: Vec<&str> = self
            .shutdown
            .stragglers()
            .iter()
            .map(|k| k.as_str())
            .collect();
        Some(format!(
            "{} が応答しなかったため強制終了しました",
            names.join(" / ")
        ))
    }

    /// (r.md #61) **我々が spawn した** VOICEVOX engine を止める。
    ///
    /// ユーザーが自分で立ち上げていた engine は `spawned_engine` が `None` の
    /// ままなので触らない (`ensure_voicevox_engine` は `is_running()` が false
    /// のときしか spawn しない)。
    ///
    /// engine は状態を持たない HTTP サーバで、公式に graceful shutdown の
    /// エンドポイントを持たない。したがって `kill` が正しい終わり方
    /// (プラグインのような「畳む手順」が存在しない)。Job Object は取りこぼし
    /// (kill 失敗) の backstop として残る。
    fn stop_spawned_voicevox_engine(&mut self) {
        let slot = std::sync::Arc::clone(&self.voicevox.spawned_engine);
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        // **先に旗を立てる**。`is_running()` の HTTP タイムアウトを待っている
        // launcher スレッドが後から spawn に成功したとき、Job が閉じた後で
        // 孤児にならないよう自分で殺してもらう。
        guard.shutting_down = true;
        let Some(mut child) = guard.child.take() else {
            return;
        };
        match child.kill() {
            Ok(()) => {
                // Windows では reap 不要だが、handle を閉じる前に status を
                // 確定させておく (ゾンビ判定のログを正確にする)。
                let _ = child.wait();
                tracing::info!("stopped the VOICEVOX engine we spawned");
            }
            Err(e) => tracing::warn!(error = ?e, "failed to stop the VOICEVOX engine"),
        }
    }

    /// respawn を抑止すべきか。終了シーケンス中の切断は「crash」ではなく
    /// **こちらが頼んだ結果**なので、子を生き返らせてはいけない。
    #[must_use]
    pub(crate) fn suppress_child_respawn(&self, kind: ChildKind) -> bool {
        if self.shutdown.is_shutting_down() {
            tracing::info!(?kind, "child disconnected during shutdown; not respawning");
            return true;
        }
        false
    }
}
