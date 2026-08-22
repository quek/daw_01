// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! マスター出力サンプルの共有メモリリング (r.md #50)。
//!
//! `AudioBridge` / `MetricsBridge` と同じ流儀で daw_gui (親) が `create`、
//! daw_audio が `open` する 3 枚目のテレメトリ面。ただし運ぶものが違う:
//! こちらは **スカラーではなくサンプル列そのもの**で、daw_gui 側の
//! `MasterAnalyzer` がオシロスコープ / スペクトラム / ゴニオ / ラウドネスを
//! すべてここから導出する (計測の SSoT は 1 つ)。
//!
//! 書き手は daw_audio の RT スレッド 1 本、読み手は daw_gui のテレメトリ
//! ポーラ 1 本。**上書きリング**なので、読み手が一周ぶん (= `SCOPE_FRAMES`)
//! 止まると古いフレームは失われる。失われたこと自体は `write_frames` の
//! 差分で検出できる ([`ScopeReader::read`] が `overrun` を返す)。
//!
//! RT 側がやるのは事前確保済みリングへの `Relaxed` store と、最後の
//! `write_frames` への `Release` store だけ。確保・ロック・I/O は無い
//! (CLAUDE.md「Real-Time Audio の制約」)。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::shmem::NamedShmem;

/// リングが保持するフレーム数。2 の冪にして剰余をビットマスクにする。
///
/// 131072 frames = 48kHz で 2.73 秒、192kHz でも 0.68 秒。テレメトリポーラは
/// 通常 33ms・省電力中でも 250ms 間隔なので、プロセス全体が 0.68 秒以上
/// 止まらない限り取りこぼさない。
pub const SCOPE_FRAMES: usize = 1 << 17;

/// `SCOPE_FRAMES` の剰余用マスク。
const SCOPE_MASK: u64 = (SCOPE_FRAMES as u64) - 1;

/// 1 回の [`ScopeReader::read`] が返すフレーム数の上限。省電力から復帰した
/// 直後などにリング全周ぶんを一気に解析しないためのガード (解析器は
/// 「取りこぼしたぶんは無音扱い」で先へ進む)。
///
/// **サンプルレートに追従させる** (固定 48000 にすると、192kHz + 省電力の
/// 250ms ポーリングで毎ティック上限を超え、恒常的に取りこぼす)。1 秒ぶんを
/// 上限にしつつ、リング容量は超えない。
#[must_use]
pub fn max_read_frames(sample_rate: u32) -> usize {
    let sr = if sample_rate == 0 { 48_000 } else { sample_rate } as usize;
    sr.min(SCOPE_FRAMES)
}

#[repr(C)]
pub struct ScopeBridge {
    /// 累積書き込みフレーム数 (monotonic)。writer が 1 ブロック書き終えた
    /// 最後に `Release` store し、reader は `Acquire` load してから中身を読む。
    write_frames: AtomicU64,
    /// writer が publish する実サンプルレート (Hz)。0 = 未 publish。
    sample_rate: AtomicU32,
    /// `write_frames` の 8 バイト境界を保つためのパディング。
    _pad: AtomicU32,
    /// インターリーブ `[L, R]` を `f32::to_bits` で保持するリング本体。
    samples: [[AtomicU32; 2]; SCOPE_FRAMES],
}

impl ScopeBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// 共有メモリ領域の所有ハンドル。
pub struct ScopeBridgeHandle {
    shmem: NamedShmem,
}

impl ScopeBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, ScopeBridge::SIZE)?;
        // ゼロ初期化: 全 atomic が 0 (= 無音・未書き込み) から始まる。
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, ScopeBridge::SIZE) };
        Ok(Self { shmem })
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, ScopeBridge::SIZE)?;
        Ok(Self { shmem })
    }

    fn bridge(&self) -> &ScopeBridge {
        // SAFETY: マッピングは少なくとも `SIZE` バイト (create/open が検証)、
        // `MapViewOfFile` のポインタは 64KiB 境界なので `AtomicU64` の 8 バイト
        // アラインを満たす。全フィールドが atomic = どんなビット列も有効なので、
        // マップされたバイト列は常に妥当な `ScopeBridge` であり、プロセスを
        // またぐ並行アクセスも健全。
        unsafe { &*(self.shmem.as_ptr() as *const ScopeBridge) }
    }

    /// daw_audio が起動時に 1 度だけ publish する実サンプルレート。
    pub fn set_sample_rate(&self, sr: u32) {
        self.bridge().sample_rate.store(sr, Ordering::Release);
    }

    pub fn sample_rate(&self) -> u32 {
        self.bridge().sample_rate.load(Ordering::Acquire)
    }

    /// **RT スレッド専用**。デインターリーブされた 1 ブロックをリングへ書く。
    ///
    /// 確保もロックもしない。`l` / `r` は同じ長さである必要はなく、短い方に
    /// 合わせて書く (どちらかが空なら何もしない)。
    pub fn write_block(&self, l: &[f32], r: &[f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        let b = self.bridge();
        // 書き込み開始位置は「これまでに書いた総フレーム数」。単一 writer
        // なので Relaxed load で十分 (自分しか書かない)。
        let base = b.write_frames.load(Ordering::Relaxed);
        for i in 0..n {
            let slot = &b.samples[((base + i as u64) & SCOPE_MASK) as usize];
            slot[0].store(l[i].to_bits(), Ordering::Relaxed);
            slot[1].store(r[i].to_bits(), Ordering::Relaxed);
        }
        // サンプル本体の store が先に見えることを保証してからカーソルを進める。
        b.write_frames
            .store(base + n as u64, Ordering::Release);
    }

    /// reader を作る。カーソルは「今の書き込み位置」から始まる (= 過去の
    /// 蓄積は読まない)。
    pub fn reader(&self) -> ScopeReader {
        ScopeReader {
            cursor: self.bridge().write_frames.load(Ordering::Acquire),
        }
    }

    /// 現在の累積書き込みフレーム数。テストとデバッグ用。
    pub fn write_frames(&self) -> u64 {
        self.bridge().write_frames.load(Ordering::Acquire)
    }
}

// 全フィールドが lock-free atomic で、読み手はどんな観測値も許容する。
unsafe impl Send for ScopeBridgeHandle {}
unsafe impl Sync for ScopeBridgeHandle {}

/// 単一の読み手が持つカーソル。
pub struct ScopeReader {
    cursor: u64,
}

/// [`ScopeReader::read`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOutcome {
    /// `out` に積んだフレーム数。
    pub frames: usize,
    /// リングを一周されて失われたフレーム数 (0 = 取りこぼし無し)。
    /// [`max_read_frames`] による意図的な切り捨ても含む。
    pub dropped: u64,
}

impl ScopeReader {
    /// 前回以降に書かれたフレームを `out` へ push する (`out` は clear しない)。
    ///
    /// リングを一周されていた場合は読める最古の位置まで飛ばし、飛ばした量を
    /// `dropped` で返す。
    pub fn read(&mut self, handle: &ScopeBridgeHandle, out: &mut Vec<[f32; 2]>) -> ReadOutcome {
        let b = handle.bridge();
        let write = b.write_frames.load(Ordering::Acquire);
        if write <= self.cursor {
            // 巻き戻ることは無い (monotonic) が、writer プロセスが再起動して
            // 0 に戻った場合だけ追従する。
            self.cursor = write;
            return ReadOutcome { frames: 0, dropped: 0 };
        }
        let available = write - self.cursor;
        let oldest = write.saturating_sub(SCOPE_FRAMES as u64);
        let cap = max_read_frames(handle.sample_rate()) as u64;
        let mut dropped = 0;
        let mut start = self.cursor;
        if start < oldest {
            dropped += oldest - start;
            start = oldest;
        }
        if write - start > cap {
            let skip = (write - start) - cap;
            dropped += skip;
            start += skip;
        }
        debug_assert!(available >= write - start);
        for i in start..write {
            let slot = &b.samples[(i & SCOPE_MASK) as usize];
            out.push([
                f32::from_bits(slot[0].load(Ordering::Relaxed)),
                f32::from_bits(slot[1].load(Ordering::Relaxed)),
            ]);
        }
        let frames = (write - start) as usize;
        self.cursor = write;
        ReadOutcome { frames, dropped }
    }
}

pub fn scope_shmem_id(parent_pid: u32) -> String {
    format!("daw_01_scope_{parent_pid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> ScopeBridgeHandle {
        // テストごとに一意な名前 (同一プロセス内の並列テストで衝突しない)。
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let i = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        ScopeBridgeHandle::create(&format!("daw_01_scope_test_{pid}_{i}")).unwrap()
    }

    #[test]
    fn write_then_read_round_trips_in_order() {
        let h = handle();
        let mut r = h.reader();
        h.write_block(&[0.1, 0.2, 0.3], &[-0.1, -0.2, -0.3]);
        let mut out = Vec::new();
        let o = r.read(&h, &mut out);
        assert_eq!(o.frames, 3);
        assert_eq!(o.dropped, 0);
        assert_eq!(out, vec![[0.1, -0.1], [0.2, -0.2], [0.3, -0.3]]);
    }

    #[test]
    fn read_without_new_frames_returns_nothing() {
        let h = handle();
        let mut r = h.reader();
        h.write_block(&[1.0], &[1.0]);
        let mut out = Vec::new();
        r.read(&h, &mut out);
        out.clear();
        let o = r.read(&h, &mut out);
        assert_eq!(o.frames, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn reader_starts_at_current_write_position() {
        let h = handle();
        h.write_block(&[1.0, 2.0], &[1.0, 2.0]);
        // reader を後から作ると、それ以前のフレームは読まない。
        let mut r = h.reader();
        let mut out = Vec::new();
        assert_eq!(r.read(&h, &mut out).frames, 0);
        h.write_block(&[3.0], &[3.0]);
        assert_eq!(r.read(&h, &mut out).frames, 1);
        assert_eq!(out, vec![[3.0, 3.0]]);
    }

    #[test]
    fn overrun_drops_the_oldest_frames_and_reports_it() {
        let h = handle();
        h.set_sample_rate(48_000);
        let mut r = h.reader();
        // リング容量 + α を書いて一周させる。
        let block = vec![0.5_f32; 4096];
        let writes = SCOPE_FRAMES / block.len() + 2;
        for _ in 0..writes {
            h.write_block(&block, &block);
        }
        let total = (writes * block.len()) as u64;
        let cap = max_read_frames(48_000);
        let mut out = Vec::new();
        let o = r.read(&h, &mut out);
        // 読めるのは最新 1 秒ぶんまで。残りは dropped。
        assert_eq!(o.frames, cap);
        assert_eq!(o.dropped, total - cap as u64);
        assert_eq!(out.len(), cap);
    }

    /// 高サンプルレートでも「1 秒ぶん」を読めること。
    ///
    /// 上限を 48000 固定にすると、192kHz + 省電力の 250ms ポーリング
    /// (= 1 ティック 48000 フレーム) で毎回上限に張り付き、恒常的に
    /// 取りこぼして積算ラウドネス / 最大トゥルーピークが過小になる。
    #[test]
    fn the_read_cap_follows_the_sample_rate() {
        assert_eq!(max_read_frames(48_000), 48_000);
        assert_eq!(max_read_frames(192_000), 192_000.min(SCOPE_FRAMES));
        // 未 publish (0) は 48kHz 相当にフォールバック。
        assert_eq!(max_read_frames(0), 48_000);

        let h = handle();
        h.set_sample_rate(192_000);
        let mut r = h.reader();
        let block = vec![0.25_f32; 48_000];
        h.write_block(&block, &block);
        let mut out = Vec::new();
        let o = r.read(&h, &mut out);
        assert_eq!(o.dropped, 0, "192kHz の 250ms ぶんを取りこぼしている");
        assert_eq!(o.frames, 48_000);
    }

    #[test]
    fn wrapping_preserves_the_newest_samples() {
        let h = handle();
        let mut r = h.reader();
        // リングちょうど 1 周ぶんの後に 3 フレーム書くと、その 3 つが最新。
        let filler = vec![0.0_f32; SCOPE_FRAMES];
        h.write_block(&filler, &filler);
        let mut out = Vec::new();
        r.read(&h, &mut out);
        out.clear();
        h.write_block(&[7.0, 8.0, 9.0], &[-7.0, -8.0, -9.0]);
        let o = r.read(&h, &mut out);
        assert_eq!(o.frames, 3);
        assert_eq!(out, vec![[7.0, -7.0], [8.0, -8.0], [9.0, -9.0]]);
    }

    #[test]
    fn empty_block_is_a_noop() {
        let h = handle();
        h.write_block(&[], &[]);
        assert_eq!(h.write_frames(), 0);
    }

    #[test]
    fn mismatched_channel_lengths_use_the_shorter_one() {
        let h = handle();
        let mut r = h.reader();
        h.write_block(&[1.0, 2.0, 3.0], &[1.0, 2.0]);
        let mut out = Vec::new();
        assert_eq!(r.read(&h, &mut out).frames, 2);
    }

}
