//! 対話 GUI プロセスの single-instance ゲート。
//!
//! 2 つ目の daw_gui を起動しようとしたら、 既に開いているウィンドウを前面化
//! して新プロセスは即終了する (VS Code / 一般的なデスクトップアプリと同じ)。
//!
//! 仕組み (Windows):
//! - **ゲート**: per-session の named mutex を `main()` 冒頭で取得する。
//!   `CreateMutexW` は同名なら既存オブジェクトへのハンドルを返し
//!   `GetLastError() == ERROR_ALREADY_EXISTS` になる (race-free)。 既存ありなら
//!   2 つ目だと判定する。 名前は prefix 無し = session-local namespace なので、
//!   別ユーザー / RDP セッションはそれぞれ 1 つ立てられる (= per-session)。
//! - **前面化チャネル**: named auto-reset event。 2 つ目はこの event を `SetEvent`
//!   して終了し、 primary 側の listener スレッドが `WaitForSingleObject` で受けて
//!   ウィンドウを前面化する (winit `focus_window`)。 既存 IPC が named pipe /
//!   OS event を使う流儀 (`common::win_sem` / `plugin_ref` の `win_event`) に倣う。
//!
//! `--script` / `--smoke-test[-text]` は **ゲート対象外**。 開発インスタンスを
//! 開いたまま書き出し / CI 検証を並行実行できる必要があるため (呼び出し側
//! `main.rs` で interactive 判定して acquire を skip する)。
//!
//! 非 Windows ではゲートは no-op (= 複数起動を許可)。 Windows 優先方針。

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
    };
    use windows::core::HSTRING;

    /// per-session の単一インスタンスを表す mutex 名 (prefix 無し = session-local)。
    const MUTEX_NAME: &str = "daw_01_single_instance";
    /// 「前面に出てこい」を primary へ伝える auto-reset event 名。
    const RAISE_EVENT_NAME: &str = "daw_01_raise_main_window";

    /// `acquire` の結果。
    pub enum SingleInstance {
        /// 自分が最初のインスタンス。 `Guard` を `main` の寿命だけ生かしておくと
        /// mutex がプロセス終了まで保持され、 ゲートとして機能する。
        Primary(Guard),
        /// 既に別インスタンスが動いていた (= 既存を前面化して signal 済み)。
        /// 呼び出し側はそのまま終了する。
        AlreadyRunning,
    }

    /// 取得した単一インスタンス資源の RAII ガード。 Drop で mutex / event を閉じる。
    /// `main` のスタックに置いてプロセス寿命のあいだ保持する。
    pub struct Guard {
        mutex: HANDLE,
        event: HANDLE,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.event);
                // mutex ハンドルを閉じる = singleton の放棄 (プロセス終了時)。
                let _ = CloseHandle(self.mutex);
            }
        }
    }

    /// auto-reset・初期非シグナルの named event を create-or-open する。
    /// 同名なら既存オブジェクトへのハンドルが返る (`plugin_ref::win_event` と同流儀)。
    fn create_or_open_event(name: &str) -> Result<HANDLE> {
        unsafe { CreateEventW(None, false, false, &HSTRING::from(name)) }
            .with_context(|| format!("CreateEventW {name}"))
    }

    /// 単一インスタンスゲートを取得する。 詳細はモジュール doc を参照。
    pub fn acquire() -> Result<SingleInstance> {
        // bInitialOwner=false: 所有権は要らない。 存在フラグとしてのみ使う。
        let mutex = unsafe { CreateMutexW(None, false, &HSTRING::from(MUTEX_NAME)) }
            .context("CreateMutexW daw_01 single-instance")?;
        // GetLastError は CreateMutexW の直後に読む (間に Win32 を挟まない)。
        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

        if already_running {
            // 自分は 2 つ目。 mutex ハンドルは閉じ (ゲートは primary が握っている)、
            // primary に前面化を要求してから呼び出し側で終了する。
            unsafe {
                let _ = CloseHandle(mutex);
            }
            match create_or_open_event(RAISE_EVENT_NAME) {
                Ok(ev) => unsafe {
                    let _ = SetEvent(ev);
                    let _ = CloseHandle(ev);
                },
                Err(e) => {
                    tracing::warn!(error = ?e, "couldn't signal existing instance to foreground")
                }
            }
            return Ok(SingleInstance::AlreadyRunning);
        }

        // 自分が primary。 2 つ目の起動が SetEvent できるよう、 event を先に作る。
        let event = create_or_open_event(RAISE_EVENT_NAME).context("create raise event")?;
        Ok(SingleInstance::Primary(Guard { mutex, event }))
    }

    /// primary 専用。 別インスタンスが前面化を要求するたびに `on_raise` を呼ぶ
    /// listener スレッドを起動する。 自前で event ハンドルを開く (create-or-open)
    /// ので `Guard` の寿命から独立している。
    pub fn spawn_raise_listener<F: Fn() + Send + 'static>(on_raise: F) -> Result<()> {
        let event = create_or_open_event(RAISE_EVENT_NAME).context("open raise event for listener")?;
        // HANDLE は Send でないのでスレッドへ移すため wrap。 この listener
        // スレッドがこのハンドルの唯一の所有者なので安全。
        struct SendHandle(HANDLE);
        unsafe impl Send for SendHandle {}
        let ev = SendHandle(event);

        std::thread::Builder::new()
            .name("singleton-raise".into())
            .spawn(move || {
                let ev = ev;
                loop {
                    let r = unsafe { WaitForSingleObject(ev.0, INFINITE) };
                    if r != WAIT_OBJECT_0 {
                        tracing::warn!(result = ?r, "singleton raise listener stopping");
                        break;
                    }
                    on_raise();
                }
                unsafe {
                    let _ = CloseHandle(ev.0);
                }
            })
            .context("spawn singleton-raise thread")?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::Result;

    /// 非 Windows ではゲート無し (複数起動を許可)。
    pub struct Guard;

    pub enum SingleInstance {
        Primary(Guard),
        AlreadyRunning,
    }

    pub fn acquire() -> Result<SingleInstance> {
        Ok(SingleInstance::Primary(Guard))
    }

    pub fn spawn_raise_listener<F: Fn() + Send + 'static>(_on_raise: F) -> Result<()> {
        Ok(())
    }
}

pub use platform::{Guard, SingleInstance, acquire, spawn_raise_listener};
