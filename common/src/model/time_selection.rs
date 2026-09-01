//! 時間範囲選択 — アレンジャー / ピアノロール / オーディオエディタが共有する
//! **唯一の選択プリミティブ** (`docs/plan_range_selection.md`)。
//!
//! 選択は「時間区間 × レーン集合」1 本で、クリップ選択もノート選択も
//! **その特殊形**として導出する (範囲がちょうどそのオブジェクトの占有区間に
//! 一致した状態)。 面ごとの選択集合 (`selected_clips` / `selected_notes` /
//! `selected_automation_points` / …) と面ごとのアンカーは、これに置き換わった。
//!
//! **session-only** — 保存もせず IPC も渡らないので `Serialize` / `Encode` は付けない。

use serde::{Deserialize, Serialize};

use crate::model::{AutomationLaneKey, ClipKey};

/// 範囲が掛かっている「行」。面ごとに何が行かが変わるだけで、扱いは同じ。
///
/// | 面 | 行 |
/// |---|---|
/// | アレンジャー | [`LaneRef::Track`] / [`LaneRef::Automation`] |
/// | ピアノロール | [`LaneRef::KeyTrack`] (= 鍵盤 1 音ぶんの横帯。Live の "key track") |
/// | オーディオエディタ | [`LaneRef::AudioLane`] |
///
/// 安定 id だけで住所を作る (positional index を持たない) ので、並べ替え / undo を
/// 跨いでも指す先が変わらない (アーキテクチャ不変条件 1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LaneRef {
    /// アレンジャーのトラック行 (`Track::id`)。
    Track(u32),
    /// アレンジャーのオートメーションレーン行。
    Automation(AutomationLaneKey),
    /// ピアノロールの鍵盤行 (どのクリップの何番の音か)。
    KeyTrack { clip: ClipKey, pitch: u8 },
    /// オーディオエディタの波形行 (どのクリップか)。
    AudioLane(ClipKey),
}

impl LaneRef {
    /// このレーンが属するトラック id (ピアノロール / オーディオエディタの行は
    /// 対象クリップのトラック)。
    #[must_use]
    pub fn track_id(self) -> u32 {
        match self {
            LaneRef::Track(id) => id,
            LaneRef::Automation(key) => key.track,
            LaneRef::KeyTrack { clip, .. } | LaneRef::AudioLane(clip) => clip.track_id,
        }
    }

    /// **アレンジャーの行**か (= 時間軸を持つ面の行)。`false` はエディタ内の行
    /// (ピアノロールの鍵盤行 / オーディオエディタの波形行) で、開いているクリップの
    /// 中を指すだけ。
    ///
    /// 「範囲がアレンジの面を指しているか」の判定はこの 1 式が SSoT。
    /// ランチャーのセル選択との排他 (`AppData::set_time_selection`) がここを引く —
    /// ピアノロールでノートを選んだだけでセル選択が降りると、セルを開いたまま
    /// 中をクリックした瞬間にエディタが空になる。
    #[must_use]
    pub fn is_arrangement_row(self) -> bool {
        matches!(self, LaneRef::Track(_) | LaneRef::Automation(_))
    }
}

/// 選択の SSoT。
///
/// `start_beat < end_beat` を常に満たす (幅ゼロは `Option::None` で表す —
/// daw_01 は Live の insert marker を持たず、範囲は再生位置に関与しない)。
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSelection {
    /// song-absolute 拍 (ピアノロール / オーディオエディタでも song 絶対拍で持つ。
    /// content-local への換算は `Clip::song_to_content_beat` が唯一の口)。
    pub start_beat: f64,
    pub end_beat: f64,
    /// 掛かっている行。**任意集合 (非連続でよい)**。空なら選択なしと同じ。
    pub lanes: Vec<LaneRef>,
}

impl TimeSelection {
    /// 端の順序を正規化して作る。幅がゼロ (以下) か、レーンが空なら `None`。
    #[must_use]
    pub fn new(a: f64, b: f64, lanes: Vec<LaneRef>) -> Option<Self> {
        let (start_beat, end_beat) = if a <= b { (a, b) } else { (b, a) };
        if lanes.is_empty() || end_beat - start_beat <= f64::EPSILON {
            return None;
        }
        Some(Self { start_beat, end_beat, lanes })
    }

    /// 区間の長さ (拍)。
    #[must_use]
    pub fn len_beats(&self) -> f64 {
        self.end_beat - self.start_beat
    }

    /// この行に範囲が掛かっているか。
    #[must_use]
    pub fn has_lane(&self, lane: LaneRef) -> bool {
        self.lanes.contains(&lane)
    }

    /// このトラックに (どの行であれ) 範囲が掛かっているか。
    #[must_use]
    pub fn has_track(&self, track_id: u32) -> bool {
        self.lanes.iter().any(|l| l.track_id() == track_id)
    }

    /// `[start, start+len)` と**交差**するか (端が触れるだけは交差しない)。
    /// クリップ属性操作 (改名 / 色 / インスペクタ) の対象判定はこちら。
    #[must_use]
    pub fn intersects(&self, start: f64, len: f64) -> bool {
        start < self.end_beat && start + len > self.start_beat
    }

    /// `[start, start+len)` を**完全に含む**か。
    #[must_use]
    pub fn contains_span(&self, start: f64, len: f64) -> bool {
        start >= self.start_beat - EPS && start + len <= self.end_beat + EPS
    }

    /// 1 点を含むか。
    #[must_use]
    pub fn contains_beat(&self, beat: f64) -> bool {
        beat >= self.start_beat - EPS && beat < self.end_beat + EPS
    }

    /// 別の区間 / レーン群を取り込んで外接まで広げる (Ctrl+クリックでの追加)。
    /// アレンジャーで離れた 2 クリップを拾うと**間のクリップも入る** — Live 実機と同じ。
    pub fn extend(&mut self, start: f64, end: f64, lanes: impl IntoIterator<Item = LaneRef>) {
        self.start_beat = self.start_beat.min(start.min(end));
        self.end_beat = self.end_beat.max(start.max(end));
        for lane in lanes {
            if !self.lanes.contains(&lane) {
                self.lanes.push(lane);
            }
        }
    }

    /// **トラック行** ([`LaneRef::Track`]) として掛かっているトラック id。
    ///
    /// [`Self::track_ids`] との違いは、オートメーションレーン行 / 鍵盤行しか
    /// 掛かっていないトラックを**含まない**こと。 クリップを動かす / 複製する側は
    /// 必ずこちらを使う — レーン行だけ選んだトラックのクリップまで動かしてしまう。
    pub fn track_row_ids(&self) -> impl Iterator<Item = u32> + '_ {
        let mut seen: Vec<u32> = Vec::new();
        self.lanes.iter().filter_map(move |l| match l {
            LaneRef::Track(id) if !seen.contains(id) => {
                seen.push(*id);
                Some(*id)
            }
            _ => None,
        })
    }

    /// 掛かっているトラック id を重複なく列挙する。
    pub fn track_ids(&self) -> impl Iterator<Item = u32> + '_ {
        let mut seen: Vec<u32> = Vec::new();
        self.lanes.iter().filter_map(move |l| {
            let id = l.track_id();
            if seen.contains(&id) {
                None
            } else {
                seen.push(id);
                Some(id)
            }
        })
    }
}

/// 拍の同一視イプシロン (包含判定の境界を安定させる)。
const EPS: f64 = 1e-9;
