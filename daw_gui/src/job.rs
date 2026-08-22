// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::process::Child;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// 子プロセスの **backstop**。`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` を立てて
/// あるので、handle を閉じた瞬間に Job 内の全プロセスが `TerminateProcess`
/// される (MSDN: "Causes all processes associated with the job to terminate
/// when the last handle to the job is closed")。
///
/// (r.md #61) **これは正常終了の主経路ではない**。正常終了では
/// [`crate::shutdown`] のシーケンスが子へ `Shutdown` を送り、子が自分で
/// プラグインを畳んで exit する。Job が実際に何かを殺すのは
///
/// - daw_gui が crash / hang して [`Self::close`] に到達しなかった場合
/// - 子が [`crate::shutdown::DRAIN_TIMEOUT`] 内に exit しなかった場合
/// - `std::mem::forget` されている VOICEVOX engine (= 明示 kill の口が無い)
///
/// の 3 つだけ。
pub struct JobHandle {
    /// `close()` を `&self` で呼べるように内部可変。**閉じたら `None`** に
    /// なるので二重 `CloseHandle` が起きない。
    ///
    /// 旧実装は `handle: HANDLE` を素で持ち Drop でのみ閉じていたため、
    /// 「いつ子が kill されるか」が `Arc<JobHandle>` の refcount
    /// (`Bootstrap.job` / `ChildSupervisor.job` / `Win32JobDispatcher` の 3 owner)
    /// という暗黙知に依存していた。明示 `close()` でその依存を消す。
    handle: Mutex<Option<HANDLE>>,
}

// `HANDLE` は `*mut c_void`。Mutex で排他しており、Win32 の job API は
// 我々が行う操作についてスレッドセーフ。handle をデリファレンスすることは無い。
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl JobHandle {
    pub fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(None, None) }.context("CreateJobObjectW failed")?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let info_size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .expect("JOBOBJECT_EXTENDED_LIMIT_INFORMATION size fits in u32");

        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                info_size,
            )
        }
        .context("SetInformationJobObject failed")?;

        Ok(Self {
            handle: Mutex::new(Some(handle)),
        })
    }

    /// 生 handle を借りて `f` を実行する。既に `close()` 済みなら `Err`。
    fn with_handle<R>(&self, what: &str, f: impl FnOnce(HANDLE) -> Result<R>) -> Result<R> {
        let guard = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        let handle = guard
            .ok_or_else(|| anyhow::anyhow!("job object already closed ({what})"))?;
        f(handle)
    }

    pub fn assign(&self, child: &Child) -> Result<()> {
        let raw = child
            .raw_handle()
            .ok_or_else(|| anyhow::anyhow!("child has no Windows HANDLE"))?;
        self.with_handle("assign", |job| {
            unsafe { AssignProcessToJobObject(job, HANDLE(raw)) }
                .context("AssignProcessToJobObject failed")
        })
    }

    /// std::process::Child 用の同等 helper。 VOICEVOX engine など、 tokio
    /// runtime に依存しない subprocess に使う。
    pub fn assign_std(&self, child: &std::process::Child) -> Result<()> {
        let raw = child.as_raw_handle();
        self.with_handle("assign_std", |job| {
            unsafe { AssignProcessToJobObject(job, HANDLE(raw)) }
                .context("AssignProcessToJobObject failed")
        })
    }

    /// (r.md #61) Job を閉じる = **残っているプロセスを強制終了する**。
    /// 冪等 (2 度目以降は no-op)。
    ///
    /// 正常終了ではここに来る時点で daw_audio / daw_plugin_host は既に
    /// 自力 exit しているので空振りする。実際に効くのは VOICEVOX engine
    /// (handle を手放しているので他に止める手段が無い) と、期限内に
    /// 終われなかった子だけ。
    pub fn close(&self) {
        let mut guard = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.take() {
            tracing::info!("closing job object (backstop kill for anything still alive)");
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // 明示 `close()` を忘れた経路 (panic / script mode の早期 return) の保険。
        self.close();
    }
}
