//! 1 buffer のグラフの**内訳**計測 (RT-safe)。
//!
//! DSP 負荷が高いときに、その時間が
//!
//! - **グラフ自身の仕事** (sequencer / mixer / strip / 内蔵 device / 合流) に行ったのか、
//! - **plugin_host の完了待ち** (`dispatch_bounded` の中) に行ったのか、
//! - どちらでもない **待ち** (runner に仕事が回ってこない / 起床待ち) なのか
//!
//! を分ける。直す場所が 3 つとも違うので、合計値 (DSP load) だけでは動けない。
//!
//! # なぜ要るか
//!
//! 2026-09-21 の実測: Analog Lab V 1 本の `process()` は 480 frame で 800 µs
//! (= 予算の 8%)、30 本を素で並列に回すと 1 本 1,231 µs (実効 19.5 コア)。
//! ところが実エンジンで 40 本回すと 1 本 2,277 µs、実効 6.6 コアまで落ちる。
//! **プラグインとマシンは出せるのに、エンジンが 1/3 しか引き出せていない。**
//! その差がどこに消えているかは、per-plugin の `process()` 時間 (plugin_host 側の
//! [`common::metrics_bridge::PluginMetricsPlane`]) だけでは決まらない — audio 側で
//! 「待っていた時間」を別に数える必要がある。
//!
//! # 測り方
//!
//! runner ごとに cache line を分けた counter へ加算する (取り合わない)。読み出しは
//! callback スレッドが buffer の最後に 1 回 ([`GraphProfile::take`]) — このとき
//! runner は全員グラフを抜けている (`AudioWorkerPool::run` が `inside` 0 を待つ)。
//!
//! RT 制約: `Instant::now()` (= `QueryPerformanceCounter`、実測 ~25 ns) と atomic の
//! 加算だけ。確保・ロック・I/O は無い。1 buffer の手の数は 40 track で ~120 なので
//! 計測自体の上乗せは ~7 µs / 10 ms buffer = 0.07%。

use std::sync::atomic::{AtomicU64, Ordering};

/// runner 1 本ぶんの内訳。**書き手はその runner だけ**なので cache line を分ける。
#[derive(Default)]
#[repr(align(64))]
struct RunnerProfile {
    /// `run_step` の中にいた時間の合計 (ns)。
    busy_ns: AtomicU64,
    /// そのうち plugin の完了待ち (`dispatch_bounded`) に使った時間の合計 (ns)。
    dispatch_ns: AtomicU64,
    /// 完了待ちの回数。
    dispatches: AtomicU64,
    /// 実行した手の数。
    steps: AtomicU64,
}

impl RunnerProfile {
    /// 書き手は 1 スレッドだけなので read-modify-write でよい (fetch_add の
    /// lock 付き命令を毎手だけ避ける)。
    #[inline]
    fn add(slot: &AtomicU64, v: u64) {
        slot.store(slot.load(Ordering::Relaxed).wrapping_add(v), Ordering::Relaxed);
    }
}

/// 1 窓ぶんの合計は [`common::metrics_bridge::GraphBreakdown`] をそのまま使う。
/// **プロセス境界を渡る型と別に「audio 側の型」を作らない** — 同じ導出
/// (`engine = busy - dispatch`、実効並列度) が 2 箇所に生えると必ず食い違う。
pub use common::metrics_bridge::GraphBreakdown;

/// グラフの内訳カウンタ。[`crate::engine_shared::WorkerRig`] が 1 つ持つ
/// (= runner の数が決まるのと同じ場所・同じ寿命)。
pub struct GraphProfile {
    runners: Vec<RunnerProfile>,
    /// callback スレッドが測るグラフ全体の壁時計 (ns)。
    wall_ns: AtomicU64,
    /// 上が何 buffer ぶんか。
    buffers: AtomicU64,
}

impl GraphProfile {
    #[must_use]
    pub fn new(runners: usize) -> Self {
        Self {
            runners: (0..runners.max(1)).map(|_| RunnerProfile::default()).collect(),
            wall_ns: AtomicU64::new(0),
            buffers: AtomicU64::new(0),
        }
    }

    /// runner `slot` が 1 手を `ns` で終えた。
    #[inline]
    pub fn add_step(&self, slot: usize, ns: u64) {
        if let Some(r) = self.runners.get(slot) {
            RunnerProfile::add(&r.busy_ns, ns);
            RunnerProfile::add(&r.steps, 1);
        }
    }

    /// runner `slot` が plugin の完了を `ns` 待った。
    #[inline]
    pub fn add_dispatch(&self, slot: usize, ns: u64) {
        if let Some(r) = self.runners.get(slot) {
            RunnerProfile::add(&r.dispatch_ns, ns);
            RunnerProfile::add(&r.dispatches, 1);
        }
    }

    /// 1 buffer ぶんのグラフが `ns` で終わった (callback スレッド)。
    #[inline]
    pub fn add_buffer(&self, ns: u64) {
        self.wall_ns.fetch_add(ns, Ordering::Relaxed);
        self.buffers.fetch_add(1, Ordering::Relaxed);
    }

    /// 溜まったぶんを取り出して 0 に戻す。**callback スレッドが buffer の最後に呼ぶ**
    /// (runner は全員グラフを抜けている)。
    pub fn take(&self) -> GraphBreakdown {
        let mut s = GraphBreakdown {
            wall_ns: self.wall_ns.swap(0, Ordering::Relaxed),
            buffers: self.buffers.swap(0, Ordering::Relaxed),
            ..GraphBreakdown::default()
        };
        for r in &self.runners {
            s.busy_ns += r.busy_ns.swap(0, Ordering::Relaxed);
            s.dispatch_ns += r.dispatch_ns.swap(0, Ordering::Relaxed);
            s.dispatches += r.dispatches.swap(0, Ordering::Relaxed);
            s.steps += r.steps.swap(0, Ordering::Relaxed);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{GraphBreakdown, GraphProfile};

    #[test]
    fn take_sums_every_runner_and_resets() {
        let p = GraphProfile::new(4);
        p.add_buffer(1_000);
        for slot in 0..4 {
            p.add_step(slot, 100);
            p.add_dispatch(slot, 60);
        }
        // 範囲外の runner は落とす (借り替えで slot が伸びても壊れない)。
        p.add_step(99, 1_000_000);

        let s = p.take();
        assert_eq!(
            s,
            GraphBreakdown {
                wall_ns: 1_000,
                busy_ns: 400,
                dispatch_ns: 240,
                dispatches: 4,
                steps: 4,
                buffers: 1,
            }
        );
        assert_eq!(s.engine_ns(), 160, "engine = busy - dispatch");
        assert!((s.concurrency() - 0.4).abs() < 1e-6);
        assert_eq!(p.take(), GraphBreakdown::default(), "take で 0 に戻る");
    }
}
