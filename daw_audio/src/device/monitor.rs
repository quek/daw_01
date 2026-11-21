//! 入力を別のスレッドへ渡す ring (wait-free の SPSC = rtrb)。使い道は 2 つ:
//!
//! - **コールバック → 描き手** (モニター用、`super::input::DeviceIo::input`)。描き手はコールバックの中か先読みの
//!   描画スレッドのどちらか 1 本。時計は同じ (どちらも `super::input` の通し番号) なので、**番号で合わせる**
//!   ([`MonitorInput::take`]): 「この出力ブロックに混ぜる入力は通し番号 N から」を描き手が指定し、それより古い
//!   分は捨て、まだ届いていない分は無音にする。先読みの有無が切り替わっても、描き手が遅れても、次の回で
//!   正しい位置に戻る。
//! - **入力デバイスのコールバック → 出力デバイスのコールバック** (OS 標準で入力と出力が別デバイスのとき)。
//!   時計が別で番号では合わせられないので、**溜まり具合で合わせる** ([`MonitorInput::take_unsynced`])。
//!
//! 捨てた / 無音で埋めたフレーム数は metrics に数える。RT: 確保・ロックをしない (作業場は作るときに確保)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::audio_device::ChannelRange;
use common::metrics_bridge::MetricsBridgeHandle;
use common::process_data::MAX_FRAMES;

use super::input::InputBlock;

/// 書く側。
pub struct MonitorFeed {
    tx: rtrb::Producer<f32>,
    channels: ChannelRange,
    /// ring が満杯で積めなかったフレーム数 (累積)。読む側が通し番号の補正に使う。
    dropped: Arc<AtomicU64>,
}

/// 読む側。
pub struct MonitorInput {
    rx: rtrb::Consumer<f32>,
    channels: ChannelRange,
    dropped: Arc<AtomicU64>,
    seen_dropped: u64,
    /// ring の先頭のフレームの通し番号。書く側が積み損ねた分は気付いた時点で足す (積み損ねた位置より前の
    /// フレームが ring に残っている間だけ、その分ずれる — 満杯になるほど古い分なので、次の回で捨てられる)。
    next_frame: u64,
    /// planar の作業場 (`channels.count × MAX_FRAMES`)。
    scratch: Vec<f32>,
}

/// `channels` の入力を `capacity_frames` フレームまで溜められる対を作る。
pub fn monitor_pair(channels: ChannelRange, capacity_frames: usize) -> (MonitorFeed, MonitorInput) {
    let count = usize::from(channels.count);
    let (tx, rx) = rtrb::RingBuffer::new((capacity_frames * count).max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        MonitorFeed { tx, channels, dropped: Arc::clone(&dropped) },
        MonitorInput {
            rx,
            channels,
            dropped,
            seen_dropped: 0,
            next_frame: 0,
            scratch: vec![0.0; count * MAX_FRAMES],
        },
    )
}

impl MonitorFeed {
    /// このフィードが運ぶ入力 ch。
    pub fn channels(&self) -> ChannelRange {
        self.channels
    }

    /// `block` を積む (frame ごとに interleave)。満杯 (読む側が止まっている) なら積まずに数える。
    pub fn push(&mut self, block: &InputBlock<'_>) {
        let count = usize::from(self.channels.count);
        if count == 0 {
            return;
        }
        let frames = block.frames();
        let Ok(mut chunk) = self.tx.write_chunk(frames * count) else {
            self.dropped.fetch_add(frames as u64, Ordering::Relaxed);
            return;
        };
        let (a, b) = chunk.as_mut_slices();
        let split = a.len();
        for k in 0..count {
            // ブロックに無い ch は `write_chunk` が入れた既定値 (0.0 = 無音) のまま。
            let Some(src) = block.channel(self.channels.first + k as u16) else { continue };
            for (f, &s) in src.iter().enumerate() {
                let i = f * count + k;
                if i < split {
                    a[i] = s;
                } else {
                    b[i - split] = s;
                }
            }
        }
        chunk.commit_all();
    }
}

impl MonitorInput {
    /// 通し番号 `want_start` から `frames` フレームぶん (`MAX_FRAMES` 以下) を取り出す (module doc)。
    pub fn take(&mut self, frames: usize, want_start: u64, metrics: &MetricsBridgeHandle) -> InputBlock<'_> {
        let frames = frames.min(MAX_FRAMES);
        if self.channels.count == 0 {
            return InputBlock::new(&[], 0, frames, self.channels, want_start);
        }
        self.sync_dropped();
        let mut available = self.available();
        // 古すぎる分を捨てる。
        if want_start > self.next_frame {
            let n = (want_start - self.next_frame).min(available as u64) as usize;
            self.discard(n);
            available -= n;
            metrics.add_input_dropped(n as u64);
        }
        // 欲しい先頭が ring の先頭より前 (もう無い) なら、その分は無音。
        let pad = self.next_frame.saturating_sub(want_start).min(frames as u64) as usize;
        // ring の先頭が欲しい位置に来ているときだけ読む (まだ届いていなければ全部無音)。
        let got = if want_start + pad as u64 == self.next_frame { available.min(frames - pad) } else { 0 };
        self.fill(pad, got, frames);
        if got < frames {
            metrics.add_input_filled((frames - got) as u64);
        }
        InputBlock::new(&self.scratch, MAX_FRAMES, frames, self.channels, want_start)
    }

    /// 時計が別の入力から `frames` フレームぶん (`MAX_FRAMES` 以下) を取り出し、planar の作業場
    /// ([`Self::planar`]) に置く。
    ///
    /// 足りなければ在る分の後ろを無音にする (無音を挟んだ分だけ、以後の入力は遅れて出る = 次に足りなくなり
    /// にくくなる)。読んだ後に `2 × slack` フレームを超えて残るなら、`slack` フレームまで古い方から捨てる
    /// (時計のずれで溜まり続けるのを戻す。毎回少しずつ捨てるより、たまにまとめて捨てる方が継ぎ目が少ない)。
    pub fn take_unsynced(&mut self, frames: usize, slack: usize, metrics: Option<&MetricsBridgeHandle>) {
        let frames = frames.min(MAX_FRAMES);
        if self.channels.count == 0 {
            return;
        }
        self.sync_dropped();
        let mut available = self.available();
        if available > frames + 2 * slack {
            let n = available - frames - slack;
            self.discard(n);
            available -= n;
            if let Some(m) = metrics {
                m.add_input_dropped(n as u64);
            }
        }
        let got = available.min(frames);
        self.fill(0, got, frames);
        if got < frames
            && let Some(m) = metrics
        {
            m.add_input_filled((frames - got) as u64);
        }
    }

    /// 作業場 (planar、ch ごとに `MAX_FRAMES` 個ずつ) と、その刻み。
    pub fn planar(&self) -> (&[f32], usize) {
        (&self.scratch, MAX_FRAMES)
    }

    fn available(&self) -> usize {
        self.rx.slots() / usize::from(self.channels.count)
    }

    fn sync_dropped(&mut self) {
        let dropped = self.dropped.load(Ordering::Relaxed);
        self.next_frame += dropped - self.seen_dropped;
        self.seen_dropped = dropped;
    }

    /// ring の先頭から `frames` フレーム捨てる (在る分より多くは呼ばない)。
    fn discard(&mut self, frames: usize) {
        if frames > 0
            && let Ok(chunk) = self.rx.read_chunk(frames * usize::from(self.channels.count))
        {
            chunk.commit_all();
            self.next_frame += frames as u64;
        }
    }

    /// 作業場を「先頭 `pad` フレーム無音 → ring から `got` フレーム → 残りを `frames` まで無音」にする。
    fn fill(&mut self, pad: usize, got: usize, frames: usize) {
        let count = usize::from(self.channels.count);
        if got > 0
            && let Ok(chunk) = self.rx.read_chunk(got * count)
        {
            let (a, b) = chunk.as_slices();
            for (i, &s) in a.iter().chain(b).enumerate() {
                self.scratch[(i % count) * MAX_FRAMES + pad + i / count] = s;
            }
            chunk.commit_all();
            self.next_frame += got as u64;
        }
        for k in 0..count {
            let ch = &mut self.scratch[k * MAX_FRAMES..k * MAX_FRAMES + frames];
            ch[..pad].fill(0.0);
            ch[pad + got..].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(tag: &str) -> MetricsBridgeHandle {
        MetricsBridgeHandle::create(&format!("daw01_test_monitor_{tag}_{}", std::process::id())).unwrap()
    }

    /// ch の k 本目の、通し番号 `i` のフレーム = `k * 1000 + i`。
    fn push(feed: &mut MonitorFeed, start: u64, frames: usize) {
        let ch = feed.channels();
        let count = usize::from(ch.count);
        let mut data = vec![0.0; count * frames];
        for k in 0..count {
            for f in 0..frames {
                data[k * frames + f] = (k * 1000) as f32 + (start as usize + f) as f32;
            }
        }
        feed.push(&InputBlock::new(&data, frames, frames, ch, start));
    }

    #[test]
    fn take_returns_what_was_pushed_per_device_channel() {
        let m = metrics("plain");
        let range = ChannelRange { first: 2, count: 2 };
        let (mut feed, mut input) = monitor_pair(range, 64);
        push(&mut feed, 0, 4);
        let block = input.take(4, 0, &m);
        assert_eq!(block.channel(2), Some(&[0.0, 1.0, 2.0, 3.0][..]));
        assert_eq!(block.channel(3), Some(&[1000.0, 1001.0, 1002.0, 1003.0][..]));
        assert_eq!(block.channel(0), None);
        assert_eq!(block.device_frame(), 0);
        assert_eq!((m.input_dropped(), m.input_filled()), (0, 0));
    }

    /// 番号で合わせる: 古い分は捨て、もう無い分とまだ届いていない分は無音。
    #[test]
    fn take_aligns_by_frame_number() {
        let m = metrics("align");
        let (mut feed, mut input) = monitor_pair(ChannelRange { first: 0, count: 1 }, 64);
        // 先読みを始めた直後: 欲しいのは 4 から、届いているのは 0..8 → 0..4 を捨てて 4..8。
        push(&mut feed, 0, 8);
        assert_eq!(input.take(4, 4, &m).channel(0), Some(&[4.0, 5.0, 6.0, 7.0][..]));
        assert_eq!((m.input_dropped(), m.input_filled()), (4, 0));
        // まだ届いていない (欲しいのは 8 から、ring は空) → 全部無音。届いたら続きから読める。
        assert_eq!(input.take(4, 8, &m).channel(0), Some(&[0.0; 4][..]));
        assert_eq!(m.input_filled(), 4);
        push(&mut feed, 8, 8);
        // 途中まで届いている (欲しいのは 12 から 8 フレーム、在るのは 16 の手前まで) → 後ろが無音。
        assert_eq!(input.take(8, 12, &m).channel(0), Some(&[12.0, 13.0, 14.0, 15.0, 0.0, 0.0, 0.0, 0.0][..]));
        // 欲しい先頭がもう無い (ring の先頭は 16、欲しいのは 14 から) → 先頭 2 フレームが無音。
        push(&mut feed, 16, 4);
        let block = input.take(4, 14, &m);
        assert_eq!(block.channel(0), Some(&[0.0, 0.0, 16.0, 17.0][..]));
        assert_eq!(block.device_frame(), 14);
    }

    /// 書く側が積み損ねた分 (満杯) は、読む側の通し番号に足される。
    #[test]
    fn frames_lost_on_a_full_ring_advance_the_frame_number() {
        let m = metrics("full");
        let (mut feed, mut input) = monitor_pair(ChannelRange { first: 0, count: 1 }, 4);
        push(&mut feed, 0, 4);
        push(&mut feed, 4, 4); // 満杯で積めない
        assert_eq!(input.take(4, 8, &m).channel(0), Some(&[0.0; 4][..]), "古い 0..4 は捨てる");
        push(&mut feed, 8, 4);
        assert_eq!(input.take(4, 8, &m).channel(0), Some(&[8.0, 9.0, 10.0, 11.0][..]));
    }

    #[test]
    fn unsynced_take_pads_short_input_and_trims_backlog() {
        let m = metrics("unsynced");
        let (mut feed, mut input) = monitor_pair(ChannelRange { first: 0, count: 1 }, 64);
        // 足りない: 2 フレームしか無いのに 4 フレーム取る → 後ろ 2 フレームが無音。
        push(&mut feed, 0, 2);
        input.take_unsynced(4, 2, Some(&m));
        assert_eq!(input.planar().0[..4], [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(m.input_filled(), 2);
        // 溜まり過ぎ: 12 フレームあって 4 フレーム取る (slack 2 → 4 + 2×2 を超える) → 残りが 2 になるまで捨てる。
        push(&mut feed, 2, 12);
        input.take_unsynced(4, 2, Some(&m));
        assert_eq!(input.planar().0[..4], [8.0, 9.0, 10.0, 11.0]);
        assert_eq!(m.input_dropped(), 6);
        // 範囲内 (残り 2 + 新しい 4 = 6 ≤ 4 + 4) なら捨てない。
        push(&mut feed, 14, 4);
        input.take_unsynced(4, 2, Some(&m));
        assert_eq!(input.planar().0[..4], [12.0, 13.0, 14.0, 15.0]);
        assert_eq!(m.input_dropped(), 6);
    }

    #[test]
    fn no_input_channels_still_reports_frames() {
        let m = metrics("none");
        let (mut feed, mut input) = monitor_pair(ChannelRange::default(), 64);
        push(&mut feed, 0, 4);
        let block = input.take(4, 0, &m);
        assert_eq!((block.frames(), block.channel(0)), (4, None));
    }
}
