//! デバイスのコールバックが呼ぶ **たった 1 つの入口** ([`EngineCallback`])。
//!
//! OS 標準 (cpal) / ASIO / 合成のどの backend も、自分の形式の buffer を「入力 = planar f32」「出力 = stereo
//! interleave f32」に直してここを呼ぶだけ。描画 (先読みを含む `OutputStage`)・入力の取り出し・`mute_output`・
//! MMCSS は backend ごとに書かない (`docs/plan_audio_io.md`「描画の入口は 1 つ」)。
//!
//! [`EngineCallback`] は stream より長生きする: backend は [`CallbackSlot`] を借りるだけなので、ドライバー側の
//! 変更で stream を組み直しても、描き手・描き溜め・録音の取り出し口・時計はそのまま続く。
//!
//! RT: 確保・ブロックするロック・I/O をしない (slot は `try_lock` — 取れなければ無音)。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::process_data::MAX_FRAMES;

use super::input::{InputBlock, InputTap};
use super::monitor::MonitorFeed;
use crate::engine::EngineShared;
use crate::output_render::OutputStage;

/// 出力は常に stereo (デバイスのどの ch へ置くかは backend の仕事)。
pub const OUT_CHANNELS: usize = 2;

/// backend から見たコールバックの中身。本番は [`EngineCallback`] だけ (backend の検査が engine 抜きで
/// 入出力を確かめられるように、型ではなく口で受ける)。
pub trait DeviceCallback: Send {
    /// 引数は [`run`] と同じ。RT で呼ばれる。
    fn process(&mut self, input: &[f32], stride: usize, out: &mut [f32], pre_mute: Option<&mut [f32]>);
}

/// stream が借りるコールバックの置き場。空 (`None`) の間と、取り合いになった回は無音を出す。
pub type CallbackSlot = Arc<Mutex<Option<Box<dyn DeviceCallback>>>>;

/// backend のコールバックから呼ぶ。`input` は planar (`stride` 個ずつ、使っている入力 ch の数ぶん)、`out` は
/// stereo interleave (`frames * 2`)。`pre_mute` を渡すと、`mute_output` で消す前の出力を写す (合成の loopback)。
///
/// 戻り値: engine が描いたか。`false` = コールバックがまだ置かれていない / 取り合いになった回で、無音を出した
/// (engine の時計は進んでいない)。
pub fn run(slot: &CallbackSlot, input: &[f32], stride: usize, out: &mut [f32], pre_mute: Option<&mut [f32]>) -> bool {
    // stream の組み直しは古い stream を止め切ってから行うので、取り合いは起きない (起きても無音で抜ける)。
    if let Ok(mut guard) = slot.try_lock()
        && let Some(cb) = guard.as_mut()
    {
        cb.process(input, stride, out, pre_mute);
        return true;
    }
    out.fill(0.0);
    if let Some(copy) = pre_mute {
        copy.fill(0.0);
    }
    false
}

pub struct EngineCallback {
    stage: OutputStage,
    tap: Box<dyn InputTap>,
    engine_shared: Arc<EngineShared>,
    /// `AudioSession::mute_output`: 描いた後、デバイスへ渡す直前に無音へ差し替える (メーター / scope は描画の中で
    /// 既に書かれているので、検査は値を読める)。
    mute_output: bool,
    /// 描き手へ入力を渡す ring (運ぶ ch = backend が渡してくる入力 ch。stream を組み直しても変えない)。
    monitor: MonitorFeed,
    /// コールバックに届いたフレームの通し番号 (`super::input` の時計)。
    device_frame: u64,
    /// MMCSS へ入れ済みのスレッド (stream を組み直すとコールバックのスレッドが替わる)。
    mmcss_thread: u32,
}

impl EngineCallback {
    pub fn new(
        stage: OutputStage,
        tap: Box<dyn InputTap>,
        engine_shared: Arc<EngineShared>,
        mute_output: bool,
        monitor: MonitorFeed,
    ) -> Self {
        Self { stage, tap, engine_shared, mute_output, monitor, device_frame: 0, mmcss_thread: 0 }
    }
}

impl DeviceCallback for EngineCallback {
    fn process(&mut self, input: &[f32], stride: usize, out: &mut [f32], pre_mute: Option<&mut [f32]>) {
        self.join_mmcss();
        let frames = out.len() / OUT_CHANNELS;
        let whole = InputBlock::new(input, stride, frames, self.monitor.channels(), self.device_frame);
        // デバイスの 1 回が `MAX_FRAMES` を超えるときは割って描く (engine の作業場は `MAX_FRAMES` ぶん)。
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_FRAMES);
            let block = whole.part(done, n);
            self.monitor.push(&block);
            self.stage.on_callback(&mut out[done * OUT_CHANNELS..(done + n) * OUT_CHANNELS], self.device_frame);
            // 録音の取り出しは描画の後 (`InputTap` の順番の保証)。
            self.tap.capture(&block);
            self.device_frame += n as u64;
            done += n;
        }
        if let Some(copy) = pre_mute {
            let n = copy.len().min(out.len());
            copy[..n].copy_from_slice(&out[..n]);
        }
        if self.mute_output {
            out.fill(0.0);
        }
    }
}

impl EngineCallback {
    /// E (plan §5): コールバックのスレッドを MMCSS "Pro Audio" に入れる (スレッドごとに 1 回)。RT 優先度の
    /// 方針を自プロセスに持つため、backend (cpal / ASIO ドライバー) の内部対応とは独立に行う。join handle は
    /// スレッドの寿命 = stream の寿命なので forget する (revert はスレッド終了時に OS が行う)。失敗は flag に
    /// 立て、notify thread が 1 回だけ warn する (RT では tracing 禁止)。ドライバーが既に MMCSS へ入れている
    /// スレッド (ASIO で普通) は失敗に数えない。
    fn join_mmcss(&mut self) {
        // SAFETY: TEB を読むだけの呼び出し。
        let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        if thread == self.mmcss_thread {
            return;
        }
        self.mmcss_thread = thread;
        match common::mmcss::join_pro_audio_or_already() {
            common::mmcss::Joined::Now(join) => std::mem::forget(join),
            common::mmcss::Joined::Already => {}
            common::mmcss::Joined::Failed => {
                self.engine_shared.mmcss_join_failed.store(true, Ordering::Release);
            }
        }
    }
}
