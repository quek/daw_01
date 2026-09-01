//! 変調の**制御グリッド** — buffer を刻みに割り、1 刻みぶんの値面を作る。
//!
//! 設計正本 `docs/plan_rmd_88_89_cross_modulation.md` §2.2 / §4-3 / §4-4。
//!
//! # なぜ 1 本なのか
//!
//! live (`engine.rs`) と書き出し (`export.rs`) は buffer 長が違う (device の
//! 実測長 vs 1024 固定)。変調を「buffer の頭で 1 回評価して buffer 定数として
//! 当てる」と、**同じ曲でも段差の位置が両者で違う** — 聴いた通りに書き出されない。
//! `render_master_buffer` を live/export で共有しているのと同じ理由 (アーキ不変条件 6)
//! で、刻みの割り方と値面の作り方もここ 1 本にする。
//!
//! # tick 境界は絶対 song サンプル位置に整列する
//!
//! [`ModTickIter`] が返す刻みの境界は `song_sample % tick_frames == 0` を満たす
//! (buffer の切れ目ではなく曲の頭からの絶対位置で決まる)。これが live と export を
//! 一致させる要 — buffer 長が 480 だろうと 1024 だろうと、踏む刻みの列は同じになる。
//!
//! # 第 1 便のスコープ
//!
//! いまは [`buffer_ticks`] が **buffer 全体を 1 刻み**として返す (= 従来と bit 同一)。
//! 第 2 便で core の `ModPlan` / `mod_graph::tick` が入ったら、刻み幅を
//! [`MOD_TICK_FRAMES`] へ差し替えて実際に 64 サンプルごとに評価・適用する。
//! 骨格 (絶対整列・値面の作り方・engine/export の共有) はこの便で確定している。
//!
//! RT 安全: 確保・ロック・I/O 無し。`ModSourceKind` は借用するだけで clone しない
//! (`MsegConfig.points` / `StepsConfig.values` が `Vec` なので clone は heap 確保)。

use common::mod_plane::ModPlane;

use crate::graph::Schedule;

/// 制御グリッドの刻み幅 (サンプル)。**automation のサブバッファ刻みと同じ格子**で、
/// 定義はここ 1 本 (`crate::automation` の `SUB_FRAMES` がこれを引く)。
///
/// 別々に 64 を持つと、片方の粒度を変えたときに automation の段差と変調の段差が
/// 黙ってずれる。設計正本 §2.2 が「automation の 64 frame イベントと同じ粒度に
/// 揃える」と言っているのはこの意味。
///
/// [`buffer_ticks`] が実際にこの幅で割るのは第 2 便 (core の `ModPlan` が入ってから)。
pub const MOD_TICK_FRAMES: u32 = 64;

/// buffer を制御グリッドで割った 1 コマ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModTick {
    /// 曲頭からの刻み番号 (`song_sample / tick_frames`)。buffer 境界に依存しない。
    pub index: u64,
    /// buffer 頭からの frame offset (`< frames`)。
    pub frame_offset: u32,
    /// この刻みが buffer 内で占める frame 数 (最初と最後は端数になりうる)。
    pub frames: u32,
    /// 刻みの先頭の絶対 song サンプル位置。
    pub song_sample: u64,
    /// 刻みの先頭の song 拍 (tempo automation を積分した真の拍位置)。
    pub song_beat: f64,
    /// 刻みの先頭の song 秒 (`song_sample / sample_rate`)。
    ///
    /// **拍からではなくサンプルから作る。** GUI プレビューが `beat*60/bpm` で
    /// 自作していてテンポカーブ下でズレた欠陥 (設計正本 §1-7) と同じ罠を、
    /// engine 側で再発させないための SSoT。
    pub song_secs: f64,
}

/// buffer を **絶対 song サンプル位置に整列した**刻みへ割るイテレータ。
#[derive(Debug, Clone)]
pub struct ModTickIter {
    start_sample: u64,
    frames: u32,
    tick_frames: u32,
    start_beat: f64,
    beats_per_frame: f64,
    inv_sample_rate: f64,
    /// 次に返す刻みの buffer 内 frame offset。
    cursor: u32,
}

impl ModTickIter {
    /// `start_sample` から `frames` サンプルぶんを `tick_frames` 刻みに割る。
    ///
    /// - `tick_frames == 0` は 1 として扱う (0 除算を作らない)。
    /// - 最初の刻みは `start_sample` がグリッドの途中なら端数になる。
    /// - `beats_per_frame` は当該 buffer の実効テンポ由来 (`bpm / (60 * SR)`)。
    ///   buffer 内は定数 — engine が既にそう扱っている粒度に合わせる。
    #[must_use]
    pub fn new(
        start_sample: u64,
        frames: u32,
        tick_frames: u32,
        start_beat: f64,
        beats_per_frame: f64,
        sample_rate: u32,
    ) -> Self {
        Self {
            start_sample,
            frames,
            tick_frames: tick_frames.max(1),
            start_beat,
            beats_per_frame,
            inv_sample_rate: if sample_rate == 0 {
                0.0
            } else {
                1.0 / f64::from(sample_rate)
            },
            cursor: 0,
        }
    }
}

impl Iterator for ModTickIter {
    type Item = ModTick;

    fn next(&mut self) -> Option<ModTick> {
        if self.cursor >= self.frames {
            return None;
        }
        let frame_offset = self.cursor;
        let song_sample = self.start_sample + u64::from(frame_offset);
        let tick = u64::from(self.tick_frames);
        // 次のグリッド境界までの残り (絶対位置基準 — ここが live/export 一致の要)。
        let to_boundary = tick - (song_sample % tick);
        #[allow(clippy::cast_possible_truncation)]
        let span = (to_boundary as u32).min(self.frames - frame_offset);
        self.cursor += span;
        Some(ModTick {
            index: song_sample / tick,
            frame_offset,
            frames: span,
            song_sample,
            song_beat: self.start_beat + f64::from(frame_offset) * self.beats_per_frame,
            #[allow(clippy::cast_precision_loss)]
            song_secs: song_sample as f64 * self.inv_sample_rate,
        })
    }
}

/// この buffer が踏む刻みの列。**engine と export が呼ぶ唯一の口**。
///
/// 第 1 便は buffer 全体で 1 刻み (= 従来の「頭で 1 回評価して buffer 定数」と
/// bit 同一)。第 2 便で刻み幅を [`MOD_TICK_FRAMES`] にする。
#[must_use]
pub fn buffer_ticks(
    start_sample: u64,
    frames: u32,
    start_beat: f64,
    beats_per_frame: f64,
    sample_rate: u32,
) -> ModTickIter {
    ModTickIter::new(
        start_sample,
        frames,
        frames,
        start_beat,
        beats_per_frame,
        sample_rate,
    )
}

/// 1 刻みぶんの変調値面を `out` に作る (**engine / export / sidecar 共通の 1 本**)。
///
/// generator (LFO / Random / MSEG / Steps) は刻みの song 位置から直接算出、
/// envelope follower は `Schedule::follower_slots` の `env` (= 直近 buffer の
/// 追従値) を読む。面は `ModSource::id` キー (アーキ不変条件 1)。
///
/// `out` は使い回す (`clear()` + `push()` のみなので確保は起きない)。
///
/// RT 安全: 確保・ロック・I/O 無し。
pub fn eval_plane(schedule: &Schedule, tick: ModTick, out: &mut ModPlane) {
    out.clear();
    for ((fs, kind), id) in schedule
        .follower_slots
        .iter()
        .zip(schedule.mod_kinds.iter())
        .zip(schedule.follower_keys.iter())
    {
        let v = common::modulators::generator_scalar(kind, tick.song_beat, tick.song_secs)
            .unwrap_or(fs.env);
        out.push(*id, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(start: u64, frames: u32, tick: u32) -> Vec<(u64, u32, u32)> {
        ModTickIter::new(start, frames, tick, 0.0, 0.0, 48_000)
            .map(|t| (t.index, t.frame_offset, t.frames))
            .collect()
    }

    /// **live (可変 buffer 長) と export (1024 固定) が同じ刻み列を踏む。**
    /// これが崩れると、同じ曲でも変調の段差の位置が再生と書き出しで違う
    /// (= 聴いた通りに書き出されない)。境界は buffer ではなく曲頭からの
    /// 絶対サンプル位置で決まる、というのがその担保。
    #[test]
    fn 刻み境界は_buffer_の切り方に依存しない() {
        let tick = MOD_TICK_FRAMES;
        // export: 1024 を 1 発。
        let one: Vec<u64> = ModTickIter::new(0, 1024, tick, 0.0, 0.0, 48_000)
            .map(|t| t.song_sample)
            .collect();
        // live: 480 + 544 に割れた同じ区間。
        let mut split: Vec<u64> = ModTickIter::new(0, 480, tick, 0.0, 0.0, 48_000)
            .map(|t| t.song_sample)
            .collect();
        split.extend(
            ModTickIter::new(480, 544, tick, 0.0, 0.0, 48_000).map(|t| t.song_sample),
        );
        // 480 は 64 の倍数ではないので、割れた側は境界をまたぐ刻みが 2 つに
        // 分かれる。**踏む絶対位置の集合**が一致することが要件。
        let mut uniq = split.clone();
        uniq.dedup();
        assert_eq!(uniq, split, "刻みの先頭が重複していない");
        for s in &one {
            assert!(split.contains(s), "{s} をまたいだ側が踏んでいない");
        }
        // またいだ刻みの断片を除けば、割った側も全部グリッド上か buffer 頭。
        for s in &split {
            assert!(one.contains(s) || *s == 480, "想定外の刻み位置 {s}");
        }
    }

    /// グリッドの途中から始まる buffer では、最初の刻みが端数になり
    /// 以降は境界に揃う (`index` は曲頭基準で連番)。
    #[test]
    fn グリッド途中から始まる_buffer_は最初だけ端数() {
        // 100 サンプル目から 200 サンプル、64 刻み。
        // 100 → 128 (28), 128 → 192 (64), 192 → 256 (64), 256 → 300 (44)
        assert_eq!(
            spans(100, 200, 64),
            vec![(1, 0, 28), (2, 28, 64), (3, 92, 64), (4, 156, 44)]
        );
    }

    /// 第 1 便の [`buffer_ticks`] は buffer 全体で 1 刻み
    /// (= 従来の「頭で 1 回評価」と bit 同一)。
    #[test]
    fn buffer_ticks_は今のところ_1_刻み() {
        let ticks: Vec<ModTick> = buffer_ticks(4096, 512, 8.0, 0.001, 48_000).collect();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].frame_offset, 0);
        assert_eq!(ticks[0].frames, 512);
        assert_eq!(ticks[0].song_sample, 4096);
        assert_eq!(ticks[0].song_beat, 8.0);
        assert_eq!(ticks[0].song_secs, 4096.0 / 48_000.0);
    }
}
