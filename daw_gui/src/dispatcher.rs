// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Background-thread → UI / OS 抽象。
//!
//! `AppData` は production も test も同じ trait object を保持する。
//!   - production: `WinitDispatcher` (winit `EventLoopProxy<AppEvent>` ラップ),
//!     `Win32JobDispatcher` (`JobHandle` ラップ)
//!   - test: `RecordingDispatcher` (送られた `AppEvent` を `Mutex<Vec<_>>` に蓄積),
//!     `NoopJobDispatcher`
//!
//! これにより:
//!   - production コードに `Option` の擬装や test-only 分岐が入らない
//!   - `AppData::new` の引数から winit 依存が消え、 headless 環境
//!     (CI / `cargo test`) でも `AppData` を直接構築して
//!     `handle_event` の連鎖をテストできる
//!   - 将来 dispatch 種類が増えたら trait に method を追加 (KISS のまま)
//!
//! 設計指針: trait surface は 1 method (`send` / `assign_std`) に絞り、
//! 「dispatch する」 ことだけを抽象化する。 状態管理 (lifecycle) は trait
//! の外で `Arc<Self>` の clone 数で制御する。

use std::sync::{Arc, Mutex};

use anyhow::Result;
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;

// ---------------------------------------------------------------------------
// BackgroundDispatcher: bg thread → main thread への AppEvent 投入を抽象化
// ---------------------------------------------------------------------------

/// Background スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
/// 合成 / plugin DB rescan) から main thread の event loop へ `AppEvent` を
/// dispatch するための抽象。
///
/// production では winit の `EventLoopProxy<AppEvent>` をラップ、 test では
/// 送信された event を `Mutex<Vec<_>>` に積む実装を使う。
///
/// `Send + Sync + 'static` は、 `Arc<dyn BackgroundDispatcher>` を background
/// thread に move するため。
pub trait BackgroundDispatcher: Send + Sync + 'static {
    /// `event` を main thread の event loop に送る。 失敗 (event loop 破棄済)
    /// は無視する (production では shutdown 競合、 test では未関心)。
    fn send(&self, event: AppEvent);
}

/// Production 実装: winit `EventLoopProxy<AppEvent>` をラップ。
pub struct WinitDispatcher {
    proxy: EventLoopProxy<AppEvent>,
}

impl WinitDispatcher {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        Self { proxy }
    }
}

impl BackgroundDispatcher for WinitDispatcher {
    fn send(&self, event: AppEvent) {
        let _ = self.proxy.send_event(event);
    }
}

/// Test 実装: 送られた `AppEvent` を `Mutex<Vec<_>>` に蓄積する。
/// テストで `recording.events()` で取り出して assert する。
#[derive(Default)]
pub struct RecordingDispatcher {
    events: Mutex<Vec<AppEvent>>,
}

impl RecordingDispatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 蓄積された event を取り出す (clear はしない)。
    pub fn events(&self) -> Vec<AppEvent> {
        self.events.lock().expect("recording mutex").clone()
    }

    /// 蓄積をクリアして取り出す。
    pub fn drain(&self) -> Vec<AppEvent> {
        std::mem::take(&mut *self.events.lock().expect("recording mutex"))
    }
}

impl BackgroundDispatcher for RecordingDispatcher {
    fn send(&self, event: AppEvent) {
        self.events.lock().expect("recording mutex").push(event);
    }
}

// ---------------------------------------------------------------------------
// JobDispatcher: child process を Win32 JobObject に登録する処理を抽象化
// ---------------------------------------------------------------------------

/// VOICEVOX engine 等の child process を Win32 JobObject に登録するための
/// 抽象。 production は `JobHandle::assign_std` 呼び出し、 test は no-op。
///
/// `'static` は、 lazy spawn する background thread に move するため。
pub trait JobDispatcher: Send + Sync + 'static {
    fn assign_std(&self, child: &std::process::Child) -> Result<()>;
}

/// Production 実装: `JobHandle` をラップ。
pub struct Win32JobDispatcher {
    job: Arc<crate::job::JobHandle>,
}

impl Win32JobDispatcher {
    pub fn new(job: Arc<crate::job::JobHandle>) -> Self {
        Self { job }
    }
}

impl JobDispatcher for Win32JobDispatcher {
    fn assign_std(&self, child: &std::process::Child) -> Result<()> {
        self.job.assign_std(child)
    }
}

/// Test 実装: 何もしない (テストでは VOICEVOX engine を spawn しないため
/// `assign_std` は実際には呼ばれないが、 trait object を構築するため必要)。
pub struct NoopJobDispatcher;

impl JobDispatcher for NoopJobDispatcher {
    fn assign_std(&self, _child: &std::process::Child) -> Result<()> {
        Ok(())
    }
}
