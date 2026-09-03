//! 行のセル列 ([`RowCells`]) と、発火の判断に要る値だけを抜いたセル ([`CellRef`])。
//! トラック行とレーン行で型が違うだけで規則は同じ (Q4) なので、判定は 1 本にまとめる。

use common::model::{
    FollowAction, LaunchMode, LaunchQuantize, LaunchSettings, SessionAutomationClip, SessionClip,
};

use crate::launcher::{RowPhase, is_positive};

/// 行のセル列。トラック行とレーン行で型が違うだけで規則は同じ (Q4) なので、
/// 判定は 1 本にまとめる。
#[derive(Debug, Clone, Copy)]
pub enum RowCells<'a> {
    Track(&'a [SessionClip]),
    Lane(&'a [SessionAutomationClip]),
}

impl RowCells<'_> {
    /// `clip.id` でセルを引く (行の中で一意)。
    #[must_use]
    pub fn find_by_clip(&self, clip_id: u32) -> Option<CellRef> {
        self.find(|id, _| id == clip_id)
    }

    /// 列 (`scene_id`) でセルを引く。
    #[must_use]
    pub fn find_by_scene(&self, scene_id: u32) -> Option<CellRef> {
        self.find(|_, sid| sid == scene_id)
    }

    /// この行のセルが使っている列 id を全部見る (列の占有判定用)。
    pub fn for_each_scene_id(&self, mut f: impl FnMut(u32)) {
        match self {
            Self::Track(v) => v.iter().for_each(|c| f(c.scene_id)),
            Self::Lane(v) => v.iter().for_each(|c| f(c.scene_id)),
        }
    }

    fn find(&self, pred: impl Fn(u32, u32) -> bool) -> Option<CellRef> {
        match self {
            Self::Track(v) => v
                .iter()
                .find(|c| pred(c.clip.id, c.scene_id))
                .map(|c| CellRef::of(c.scene_id, &c.launch, &c.clip.id, c.clip.start_beat, c.clip.length_beats)),
            Self::Lane(v) => v
                .iter()
                .find(|c| pred(c.clip.id, c.scene_id))
                .map(|c| CellRef::of(c.scene_id, &c.launch, &c.clip.id, c.clip.start_beat, c.clip.length_beats)),
        }
    }
}

/// セル 1 つから、発火の判断に要る値だけを抜いたもの。
#[derive(Debug, Clone)]
pub struct CellRef {
    pub clip_id: u32,
    pub scene_id: u32,
    /// セル自身の拍原点。正規化済みなら 0 だが、IPC は信頼境界なので値を持ち回る。
    pub start_beat: f64,
    pub length_beats: f64,
    pub quantize: LaunchQuantize,
    pub mode: LaunchMode,
    pub looping: bool,
    pub legato: bool,
    pub follow: FollowAction,
}

impl CellRef {
    fn of(
        scene_id: u32,
        launch: &LaunchSettings,
        clip_id: &u32,
        start_beat: f64,
        length_beats: f64,
    ) -> Self {
        Self {
            clip_id: *clip_id,
            scene_id,
            start_beat,
            length_beats,
            quantize: launch.quantize,
            mode: launch.mode,
            looping: launch.looping,
            legato: launch.legato,
            follow: launch.follow.clone(),
        }
    }

    /// このセルを `at` 拍で撃った状態。
    ///
    /// `prev` を渡すと **Legato** — 前のセルの位相 (`(前の供給元, その実効拍)`) を
    /// 引き継ぐように `launch_beat` を逆算する。同じ小節位置のまま別のループへ
    /// 乗り換えるのが Legato の定義なので、位相は移る先のセル長で折り返す。
    #[must_use]
    pub fn phase_at(&self, at: f64, prev: Option<(RowPhase, f64)>) -> RowPhase {
        let launch_beat = match prev {
            Some((RowPhase::Cell { cell_start_beat, .. }, eff)) if self.length_beats > 0.0 => {
                at - (eff - cell_start_beat).rem_euclid(self.length_beats)
            }
            _ => at,
        };
        RowPhase::Cell {
            clip_id: self.clip_id,
            launch_beat,
            loop_len: self.length_beats,
            cell_start_beat: self.start_beat,
            looping: self.looping,
        }
    }

    /// このセルを `at` 拍で **`phase` (セルの `start_beat` からの拍) の位置から**
    /// 鳴らした状態 ([`super::LaunchRequest::CellFrom`])。
    ///
    /// 起点 `launch_beat` を `phase` ぶん過去へ置く — Legato の位相引き継ぎと同じ
    /// 仕組みで、以降の [`RowPhase::effective_beat`] / フォロー / ワンショット
    /// 終端は全部この起点から普通に解ける。ループするセルは長さで折り返し、
    /// ワンショットは末尾で切る (末尾以降を指せば `end_at == at` で即座に無音)。
    #[must_use]
    pub fn phase_from(&self, at: f64, phase: f64) -> RowPhase {
        let len = self.length_beats;
        let phase = if !is_positive(len) || !phase.is_finite() {
            0.0
        } else if self.looping {
            phase.rem_euclid(len)
        } else {
            phase.clamp(0.0, len)
        };
        RowPhase::Cell {
            clip_id: self.clip_id,
            launch_beat: at - phase,
            loop_len: len,
            cell_start_beat: self.start_beat,
            looping: self.looping,
        }
    }
}
