//! plugin-main スレッドの起動・命令の受け渡し・終了と、worker が固まったときのプロセスの終え方。
//!
//! plugin-main は `crate::plugin_main_loop` を回す 1 本のスレッド。`main` (tokio) は pipe で受けた命令を
//! [`PluginThreadSender`] で渡し、終了時に [`PluginThread::shutdown`] で畳む。
//!
//! **plugin-main が自分から終わったとき** (worker が plugin の `process()` から戻らない =
//! `PluginHost::wedged` / plugin が投げた WM_QUIT / panic): 抜け方 ([`MainLoopExit`]) を
//! [`PluginThread::spawn`] に渡した口で `main` に知らせ、`main` は **プロセスごと** 終わる (wedge は
//! [`terminate_wedged`])。pipe は daw_gui が開いたままなので、プロセスが残ると daw_gui は切断に気付かず
//! respawn しない (全 plugin が「読み込み中」のまま残る。r.md #156 の実機確認で、Melodyne が固まった後に
//! デバイスを切り替えて発覚)。

use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::protocol::{AudioSession, PluginEvent};
use tokio::sync::mpsc as tmpsc;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadId, TerminateProcess};
use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

use crate::{HostMsg, WM_COMMAND_WAKE, plugin_main_loop};

/// plugin-main の loop の抜け方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainLoopExit {
    /// 終了要求 / pipe の切断 / WM_QUIT。全 plugin を畳み終えている。
    Normal,
    /// worker が plugin の `process()` から戻らない (`PluginHost::wedged`)。プロセスを終えて respawn に任せる。
    Wedged,
}

pub(crate) struct PluginThread {
    join: Option<JoinHandle<()>>,
    cmd_tx: mpsc::Sender<HostMsg>,
    thread_id: u32,
}

impl PluginThread {
    /// `exit_tx`: plugin-main が終わったときの抜け方を送る (panic なら送らずに落ちる)。
    pub(crate) fn spawn(
        session: AudioSession,
        evt_tx: tmpsc::UnboundedSender<PluginEvent>,
        exit_tx: tokio::sync::oneshot::Sender<MainLoopExit>,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostMsg>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();

        let join = std::thread::Builder::new()
            .name("plugin-main".into())
            .spawn(move || {
                // SAFETY: TEB を読むだけの呼び出し。
                let tid = unsafe { GetCurrentThreadId() };
                let _ = tid_tx.send(tid);
                let _ = exit_tx.send(plugin_main_loop(session, cmd_rx, evt_tx));
            })
            .context("failed to spawn plugin-main thread")?;

        let thread_id = tid_rx.recv().context("plugin-main thread failed to report its id")?;

        Ok(Self { join: Some(join), cmd_tx, thread_id })
    }

    pub(crate) fn sender(&self) -> PluginThreadSender {
        PluginThreadSender { cmd_tx: self.cmd_tx.clone(), thread_id: self.thread_id }
    }

    pub(crate) fn shutdown(mut self) {
        // 送れなかった = plugin-main はもう終わっている。終わったスレッドの id は別のスレッドに再利用されうる
        // ので、その場合は起こしに行かない。
        if self.cmd_tx.send(HostMsg::Shutdown).is_ok() {
            wake_thread(self.thread_id);
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct PluginThreadSender {
    cmd_tx: mpsc::Sender<HostMsg>,
    thread_id: u32,
}

impl PluginThreadSender {
    pub(crate) fn send(&self, msg: HostMsg) {
        if self.cmd_tx.send(msg).is_err() {
            tracing::warn!("plugin-main thread channel closed; command dropped");
            return;
        }
        wake_thread(self.thread_id);
    }
}

fn wake_thread(thread_id: u32) {
    // SAFETY: スレッド id と値渡しの引数だけ。宛先が既に終わっていれば失敗するだけ。
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_COMMAND_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// worker が固まったまま plugin-main が終わった後、プロセスを終える (module doc)。戻らない。
///
/// 普通の終了 (`ExitProcess`) は使わない: 固まったスレッドを止めてから DLL の後始末を呼ぶので、その
/// スレッドが握っていたロックを後始末が待って、終われないことがある。`TerminateProcess` は後始末を通さない。
/// 同じ理由で、再生中に wedge を見つけた plugin-main はほかの plugin を畳まずに抜けてくる
/// (`plugin_main_loop` の末尾)。
pub(crate) fn terminate_wedged(log_guard: impl Sized) -> ! {
    tracing::error!("a plugin worker is wedged in process(); terminating daw_plugin_host so daw_gui respawns it");
    // ログを書き切ってから終える (guard の drop が flush する)。
    drop(log_guard);
    // SAFETY: 自プロセスの擬似 handle を渡すだけ。
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), 1);
    }
    // `TerminateProcess` が失敗したときだけここへ来る。
    std::process::abort();
}
