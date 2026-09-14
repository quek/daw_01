//! 書き出し中の走行状態を [`common::launcher_sidecar::LauncherSidecar`] へ焼く記録器
//! (`docs/plan_rmd_87_clip_launcher.md` §3.6)。
//!
//! 動画書き出しは音声書き出しとは別プロセス・別の走査なので、`daw_gui` 側は
//! **フォローアクションがどこで次の列へ移ったか**を知らない。焼かないと
//! 「音は Scene2 へ移ったのに絵は Scene1 を延々ループ」で出る。GUI 側で
//! `LauncherRuntime` を再実装しないための唯一の口 (式が 2 本になるのを避ける)。
//!
//! 記録は**遷移だけ**。状態は区分定数なので、これでフレーム単位に厳密な復元ができる。
//!
//! # 呼ぶ場所
//!
//! `export.rs` の走査ループで、`LauncherRuntime::update` の直後 (= この buffer の
//! 供給元テーブルが確定した直後)。off-RT なので確保してよい。

use std::collections::HashMap;

use common::launcher_sidecar::{LauncherRowState, LauncherSidecar};

use super::runtime::BufferSpan;
use super::{RowPhase, RowSourceTable};

/// 行ごとの「最後に記録した供給元」を持ち、変わった瞬間だけ sidecar へ積む。
#[derive(Debug, Default)]
pub struct SidecarRecorder {
    /// `row_key.packed()` → 最後に記録した供給元 (毎 buffer 全行を引くので、行数の二乗にしない)。
    last: HashMap<u64, RowPhase>,
    out: LauncherSidecar,
}

impl SidecarRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 buffer 分を記録する。
    ///
    /// 供給元の切り替えは buffer の途中で起きうる ([`super::RowTimeSource::switch_frame`])
    /// ので、**切り替え拍そのもの**を stamp する。buffer 先頭へ丸めると遷移位置が
    /// device buffer size 依存になり、live と書き出しで絵がズレる。
    pub fn record(&mut self, table: &RowSourceTable, span: BufferSpan) {
        for src in table.iter() {
            let key = src.key.packed();
            // `switch_frame == 0` の行は head を一度も描かないので記録しない。
            if src.switch_frame > 0 {
                self.emit(key, src.head, span.start_beat);
            }
            let at = span.start_beat + f64::from(src.switch_frame) * span.beats_per_frame;
            self.emit(key, src.tail, at);
        }
    }

    /// 焼き上がった sidecar (呼び側が WAV の隣へ書く)。
    #[must_use]
    pub fn finish(self) -> LauncherSidecar {
        self.out
    }

    fn emit(&mut self, key: u64, phase: RowPhase, beat: f64) {
        if self.last.insert(key, phase) == Some(phase) {
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let row = LauncherRowState {
            track_id: (key >> 32) as u32,
            lane_id: (key & 0xFFFF_FFFF) as u32,
            playback: phase.playback(),
            launch_beat: phase.launch_beat(),
        };
        self.out.push(beat, row);
    }
}
