//! transport 要求 (seek / Play / Stop) とランチャー操作を **同じ buffer で** 効かせるまとまり。
//!
//! recv loop は transport 要求を 1 件ずつでも、`AudioCommand::BeginAtomic` 〜 `EndAtomic` の括りの
//! 中身でも、この形にして command ring へ **1 件として** 積む ([`super::EngineCommand::Atomic`])。
//! audio thread は buffer 頭の `pump_commands` で 1 件を丸ごと取り出すので、1 件の中身が 2 つの
//! buffer に割れることはない (以前は transport を受信スレッドが共有面の atomic へ、ランチャー操作を
//! ring へ別々に書いていて、間に 1 buffer 挟まると片方だけ先に効いた)。
//!
//! 揃わないのは 2 つだけ: ランチャー操作が枠 ([`ATOMIC_BATCH_LAUNCHES`]) を超えた括り (続きは次の
//! 1 件になる) と、窓の間 (凍結中) の project 宛て (transport は窓が閉じるまで預かり、ランチャー
//! 操作は他のコマンドと同じく捨てる — `DeviceRt::apply_atomic`)。

use std::sync::atomic::Ordering;

use common::protocol::ProjectKey;

use super::{EngineCommand, EngineCommandSender, PlaybackCommand};
use crate::engine_shared::ProjectShared;
use crate::launcher::LaunchRequest;

/// 1 つのまとまりに載るランチャー操作の数。GUI が送るのは `f` で 2 件
/// (`LaunchCellFrom` + `RephaseLauncherRows`)、1 つの MIDI 入力に割り当てた操作でその件数ぶん。
pub const ATOMIC_BATCH_LAUNCHES: usize = 8;

/// まとまりに積む 1 件。
#[derive(Debug, Clone, Copy)]
pub enum BatchItem {
    Seek(u64),
    Playback(PlaybackCommand),
    Launch(LaunchRequest),
}

/// 同じ buffer で効かせる transport 要求とランチャー操作。`Copy` で確保を伴わない。
///
/// transport は種類ごとに last-wins (共有面の atomic と同じ意味)、ランチャー操作は積んだ順。
#[derive(Debug, Clone, Copy, Default)]
pub struct AtomicBatch {
    seek: Option<u64>,
    playback: Option<PlaybackCommand>,
    launches: [Option<LaunchRequest>; ATOMIC_BATCH_LAUNCHES],
}

impl AtomicBatch {
    #[must_use]
    pub fn of(item: BatchItem) -> Self {
        let mut batch = Self::default();
        // 空のまとまりには必ず入る。
        let _ = batch.push(item);
        batch
    }

    /// 1 件積む。ランチャー操作の枠が満杯なら積まずに返す (呼び出し側がそこで割る)。
    pub fn push(&mut self, item: BatchItem) -> Result<(), BatchItem> {
        match item {
            BatchItem::Seek(samples) => self.seek = Some(samples),
            BatchItem::Playback(cmd) => self.playback = Some(cmd),
            BatchItem::Launch(req) => match self.launches.iter_mut().find(|slot| slot.is_none()) {
                Some(slot) => *slot = Some(req),
                None => return Err(item),
            },
        }
        Ok(())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seek.is_none() && self.playback.is_none() && self.launches[0].is_none()
    }

    /// transport 要求を共有面へ置く (RT、`pump_commands`)。同じ buffer の
    /// `ProjectRt::consume_transport_requests` が消費する。
    pub(crate) fn store_transport(&self, shared: &ProjectShared) {
        if let Some(samples) = self.seek {
            shared.pending_seek.store(samples, Ordering::Release);
        }
        if let Some(cmd) = self.playback {
            shared.playback.store(cmd as u8, Ordering::Release);
        }
    }

    /// ランチャー操作 (積んだ順)。
    pub(crate) fn launches(&self) -> impl Iterator<Item = LaunchRequest> + '_ {
        self.launches.iter().flatten().copied()
    }
}

/// recv loop 側の送り口 (project ごと)。transport 要求とランチャー操作を [`AtomicBatch`] にして
/// command ring へ積む。`AudioCommand::BeginAtomic` 〜 `EndAtomic` の間は溜めて、閉じたときに
/// 1 件で積む。括りの外では 1 件ずつそのまま積む。
#[derive(Debug, Default)]
pub struct BatchGate {
    open: Option<AtomicBatch>,
}

impl BatchGate {
    /// 括りを開く。前の括りが閉じられていなければ (GUI の不具合)、溜めた分を先に流す —
    /// 開きっぱなしの括りが transport 要求を飲み込み続けないように。
    pub fn begin(&mut self, project: ProjectKey, tx: &mut EngineCommandSender) {
        self.end(project, tx);
        self.open = Some(AtomicBatch::default());
    }

    /// 括りを閉じ、溜めた分を 1 件で積む。
    pub fn end(&mut self, project: ProjectKey, tx: &mut EngineCommandSender) {
        if let Some(batch) = self.open.take()
            && !batch.is_empty()
        {
            tx.send(EngineCommand::Atomic { project, batch });
        }
    }

    /// 1 件送る。括りが開いていれば溜める。ランチャー操作が枠 ([`ATOMIC_BATCH_LAUNCHES`]) を
    /// 超えたら、そこまでを積んで続きを新しいまとまりにする (割れるが、順序は保つ)。
    pub fn submit(&mut self, project: ProjectKey, tx: &mut EngineCommandSender, item: BatchItem) {
        let Some(batch) = &mut self.open else {
            tx.send(EngineCommand::Atomic { project, batch: AtomicBatch::of(item) });
            return;
        };
        if let Err(item) = batch.push(item) {
            tx.send(EngineCommand::Atomic { project, batch: std::mem::replace(batch, AtomicBatch::of(item)) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: ProjectKey = ProjectKey(3);

    fn drain(rx: &mut rtrb::Consumer<EngineCommand>) -> Vec<AtomicBatch> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.pop() {
            match cmd {
                EngineCommand::Atomic { project, batch } => {
                    assert_eq!(project, KEY);
                    out.push(batch);
                }
                other => panic!("{other:?} が来た"),
            }
        }
        out
    }

    /// 括りの中身は、閉じたときに ring の **1 件** として出る (audio thread が 2 つの buffer に
    /// 分けて読むことが無い)。括りの外は 1 件ずつそのまま出る。
    #[test]
    fn 括りの中身は_1_件にまとまる() {
        let (mut tx, mut rx) = EngineCommandSender::channel();
        let mut gate = BatchGate::default();
        let rephase = LaunchRequest::RephaseRunning { phase_beats: 1.5 };

        gate.begin(KEY, &mut tx);
        gate.submit(KEY, &mut tx, BatchItem::Seek(100));
        gate.submit(KEY, &mut tx, BatchItem::Playback(PlaybackCommand::Play));
        gate.submit(KEY, &mut tx, BatchItem::Launch(LaunchRequest::AllToArranger));
        gate.submit(KEY, &mut tx, BatchItem::Launch(rephase));
        assert!(drain(&mut rx).is_empty(), "閉じる前に出た");
        gate.end(KEY, &mut tx);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].seek, Some(100));
        assert_eq!(got[0].playback, Some(PlaybackCommand::Play));
        assert_eq!(got[0].launches().collect::<Vec<_>>(), [LaunchRequest::AllToArranger, rephase]);

        // 括りの外: 届いた順に 1 件ずつ。
        gate.submit(KEY, &mut tx, BatchItem::Playback(PlaybackCommand::Stop));
        gate.submit(KEY, &mut tx, BatchItem::Seek(7));
        let got = drain(&mut rx);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].playback, got[1].seek), (Some(PlaybackCommand::Stop), Some(7)));

        // 空の括り (再生が読み込み待ちで預かりになった) は何も出さない。
        gate.begin(KEY, &mut tx);
        gate.end(KEY, &mut tx);
        assert!(drain(&mut rx).is_empty());
    }

    /// 閉じられなかった括りは、次の括りが開くときに流れる (transport 要求を飲み込んだままに
    /// しない)。枠を超えたランチャー操作は、順序を保って次のまとまりへ続く。
    #[test]
    fn 閉じ忘れと枠あふれでも要求を失わない() {
        let (mut tx, mut rx) = EngineCommandSender::channel();
        let mut gate = BatchGate::default();

        gate.begin(KEY, &mut tx);
        gate.submit(KEY, &mut tx, BatchItem::Playback(PlaybackCommand::Stop));
        gate.begin(KEY, &mut tx);
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].playback, Some(PlaybackCommand::Stop));

        let reqs: Vec<LaunchRequest> = (0..=ATOMIC_BATCH_LAUNCHES)
            .map(|i| LaunchRequest::RephaseRunning { phase_beats: i as f64 })
            .collect();
        for req in &reqs {
            gate.submit(KEY, &mut tx, BatchItem::Launch(*req));
        }
        gate.end(KEY, &mut tx);
        let got: Vec<LaunchRequest> = drain(&mut rx).iter().flat_map(|b| b.launches().collect::<Vec<_>>()).collect();
        assert_eq!(got, reqs);
    }
}
