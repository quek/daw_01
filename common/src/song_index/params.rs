//! パラメーターの置き場 (lane 列 + routing 列) の索引。

use std::collections::HashMap;

use super::launcher::{CellIndex, CellSlice, SessionCells};
use super::ranges::CoverIndex;
use crate::model::{AutomationClip, AutomationLane, AutomationTarget, ModRouting, SessionAutomationClip, TrackBuiltinParam};

/// lane / routing を束ねる単位。RT は「device の param 全部」「chain の gain / pan」のように、この単位で引く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ParamSubject {
    /// トラック自身の Volume / Pan / Mute。
    Track,
    Send(u32),
    Chain(u64),
    Parallel(u64),
    Plugin(u64),
    Native(u64),
    MasterLimiter,
    /// テンポ / 拍子。
    Song,
    ModSource(u32),
    ModRouting(u32),
    /// 映像 (image / text / group transform)。daw_audio は評価しない。
    Visual,
}

impl ParamSubject {
    #[must_use]
    pub fn of(target: &AutomationTarget) -> Self {
        use TrackBuiltinParam as B;
        match target {
            AutomationTarget::TrackBuiltin(b) => match b {
                B::Volume | B::Pan | B::Mute => Self::Track,
                B::SendGain { send_id, .. } => Self::Send(*send_id),
                B::ChainGain { chain_id } | B::ChainPan { chain_id } => Self::Chain(*chain_id),
                B::ParallelOutGain { parallel_id }
                | B::ParallelSplitFreq { parallel_id, .. }
                | B::ParallelSelect { parallel_id } => Self::Parallel(*parallel_id),
            },
            AutomationTarget::PluginParam { device_id, .. } => Self::Plugin(*device_id),
            AutomationTarget::NativeParam { device_id, .. } => Self::Native(*device_id),
            AutomationTarget::MasterLimiter(_) => Self::MasterLimiter,
            AutomationTarget::SongTempo | AutomationTarget::SongTimeSigNumerator => Self::Song,
            AutomationTarget::ModSourceParam { source_id, .. } => Self::ModSource(*source_id),
            AutomationTarget::ModRoutingDepth { routing_id } => Self::ModRouting(*routing_id),
            AutomationTarget::ImageBuiltin(_) | AutomationTarget::TextBuiltin(_) | AutomationTarget::GroupTransform(_) => {
                Self::Visual
            }
        }
    }
}

/// 1 target を指す routing の束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoutingGroup {
    subject: ParamSubject,
    /// [`ParamStoreIndex::members`] の範囲。
    start: u32,
    len: u32,
}

/// 1 つの置き場 (lane 列 + routing 列) の索引。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParamStoreIndex {
    /// 作ったときの本数 (別の snapshot と組まれていないかの照合)。
    n_lanes: usize,
    n_routings: usize,
    /// `(subject, lane の位置)`。subject 順、同じ subject の中は置き場の並び順。
    lanes: Vec<(ParamSubject, u32)>,
    /// target ごとの routing の束。subject 順、同じ subject の中は初出順。
    groups: Vec<RoutingGroup>,
    /// 束ごとの routing の位置を連結したもの (束の中は置き場の並び順)。
    members: Vec<u32>,
    /// lane の位置 → アレンジの automation clip (`lane.clips`) の索引。
    covers: Vec<CoverIndex>,
    /// lane の位置 → セル (`lane.session_clips`) の索引。
    pub(super) cells: Vec<CellIndex>,
    /// `(lane id, lane の位置)` を id 順に (同じ id は先頭だけ)。
    ids: Vec<(u32, u32)>,
}

/// 空の置き場の索引 (`Vec::new` は確保しないので static に置ける)。
static EMPTY_STORE: ParamStoreIndex = ParamStoreIndex {
    n_lanes: 0,
    n_routings: 0,
    lanes: Vec::new(),
    groups: Vec::new(),
    members: Vec::new(),
    covers: Vec::new(),
    cells: Vec::new(),
    ids: Vec::new(),
};

pub(super) fn position(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl ParamStoreIndex {
    /// 置き場の lane / routing から作る (off-RT)。
    #[must_use]
    pub fn build(lanes: &[AutomationLane], routings: &[ModRouting]) -> Self {
        let mut lane_keys: Vec<(ParamSubject, u32)> =
            lanes.iter().enumerate().map(|(i, l)| (ParamSubject::of(&l.target), position(i))).collect();
        lane_keys.sort_unstable();

        // target → 束 (初出順)。
        let mut group_of: HashMap<&AutomationTarget, usize> = HashMap::new();
        let mut raw: Vec<(ParamSubject, Vec<u32>)> = Vec::new();
        for (i, r) in routings.iter().enumerate() {
            let g = *group_of.entry(&r.target).or_insert_with(|| {
                raw.push((ParamSubject::of(&r.target), Vec::new()));
                raw.len() - 1
            });
            raw[g].1.push(position(i));
        }
        // 安定ソートなので、同じ subject の中は初出順のまま。
        raw.sort_by_key(|(subject, _)| *subject);
        let mut groups = Vec::with_capacity(raw.len());
        let mut members = Vec::with_capacity(routings.len());
        for (subject, positions) in raw {
            groups.push(RoutingGroup { subject, start: position(members.len()), len: position(positions.len()) });
            members.extend(positions);
        }

        // `common::automation::clip_covering` と同じ規則 (長さが正で `[start, start + length)` に入る最初の clip)。
        let covers = lanes
            .iter()
            .map(|l| CoverIndex::build(l.clips.iter().map(|c| (c.start_beat, c.start_beat + c.length_beats))))
            .collect();
        let cells = lanes
            .iter()
            .map(|l| CellIndex::build(l.session_clips.iter().map(|c| (c.clip.id, c.scene_id))))
            .collect();

        Self {
            n_lanes: lanes.len(),
            n_routings: routings.len(),
            lanes: lane_keys,
            groups,
            members,
            covers,
            cells,
            ids: super::id_positions(lanes.iter().map(|l| l.id)),
        }
    }
}

/// 置き場 1 つを索引と組にした読み口 (RT 可、確保なし)。
#[derive(Debug, Clone, Copy)]
pub struct ParamStore<'a> {
    pub lanes: &'a [AutomationLane],
    pub routings: &'a [ModRouting],
    index: &'a ParamStoreIndex,
}

impl Default for ParamStore<'_> {
    fn default() -> Self {
        Self { lanes: &[], routings: &[], index: &EMPTY_STORE }
    }
}

/// 1 target を指す routing (置き場の並び順)。`Copy` なので何度でも走査できる。
#[derive(Debug, Clone, Copy, Default)]
pub struct TargetRoutings<'a> {
    routings: &'a [ModRouting],
    members: &'a [u32],
}

impl<'a> TargetRoutings<'a> {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.members.is_empty()
    }

    pub fn iter(self) -> impl Iterator<Item = &'a ModRouting> + Clone {
        self.members.iter().filter_map(move |&i| self.routings.get(i as usize))
    }
}

/// lane 1 本と、その clip / セルの索引。
#[derive(Debug, Clone, Copy)]
pub struct LaneView<'a> {
    /// 置き場の中の位置 (ランチャーの行の供給元 `TrackRows::lane` の鍵)。
    pub pos: usize,
    pub lane: &'a AutomationLane,
    cover: &'a CoverIndex,
    cells: SessionCells<'a>,
}

impl<'a> LaneView<'a> {
    /// `beat` を覆うアレンジの clip (`common::automation::lane_value_at` が引く clip と同じ)。
    #[must_use]
    pub fn arrangement_clip(self, beat: f64) -> Option<&'a AutomationClip> {
        self.cover.first_covering(beat).and_then(|i| self.lane.clips.get(i))
    }

    /// `clip.id` のセル (`session_clips.iter().find(..)` と同じもの)。
    #[must_use]
    pub fn cell(self, clip_id: u32) -> Option<&'a SessionAutomationClip> {
        self.lane.session_clips.get(self.cells.clip_pos(clip_id)?)
    }

    /// この lane のセル列。
    #[must_use]
    pub fn session_cells(self) -> SessionCells<'a> {
        self.cells
    }
}

/// `v` のうち key が `k` の連続区間 (`v` は key 順)。
pub(super) fn key_range<T, K: Ord>(v: &[T], k: K, key: impl Fn(&T) -> K) -> &[T] {
    let lo = v.partition_point(|x| key(x) < k);
    let len = v[lo..].partition_point(|x| key(x) == k);
    &v[lo..lo + len]
}

impl<'a> ParamStore<'a> {
    /// 索引と組にする。本数が合わない (= 別の snapshot の索引) なら空の索引として扱う。
    #[must_use]
    pub fn new(lanes: &'a [AutomationLane], routings: &'a [ModRouting], index: &'a ParamStoreIndex) -> Self {
        let matches = index.n_lanes == lanes.len() && index.n_routings == routings.len();
        Self { lanes, routings, index: if matches { index } else { &EMPTY_STORE } }
    }

    /// 位置 `pos` の lane。
    #[must_use]
    pub fn lane(self, pos: usize) -> Option<LaneView<'a>> {
        let lane = self.lanes.get(pos)?;
        Some(LaneView {
            pos,
            lane,
            cover: self.index.covers.get(pos)?,
            cells: SessionCells::new(CellSlice::Lane(&lane.session_clips), self.index.cells.get(pos)?),
        })
    }

    /// `subject` の lane (置き場の並び順)。
    pub fn lanes_of(self, subject: ParamSubject) -> impl Iterator<Item = LaneView<'a>> {
        key_range(&self.index.lanes, subject, |x| x.0).iter().filter_map(move |&(_, i)| self.lane(i as usize))
    }

    /// `target` の lane のうち、置き場の並び順で最初の有効なもの。
    #[must_use]
    pub fn enabled_lane(self, target: &AutomationTarget) -> Option<LaneView<'a>> {
        self.lanes_of(ParamSubject::of(target)).find(|v| v.lane.enabled && &v.lane.target == target)
    }

    /// lane id の lane (`lanes.iter().find(|l| l.id == id)` と同じもの)。
    #[must_use]
    pub fn lane_by_id(self, id: u32) -> Option<LaneView<'a>> {
        let i = self.index.ids.binary_search_by_key(&id, |&(lid, _)| lid).ok()?;
        self.lane(self.index.ids[i].1 as usize)
    }

    /// `subject` の target ごとの routing (初出順)。
    pub fn targets_of(self, subject: ParamSubject) -> impl Iterator<Item = (&'a AutomationTarget, TargetRoutings<'a>)> {
        key_range(&self.index.groups, subject, |g| g.subject).iter().filter_map(move |g| {
            let members = self.index.members.get(g.start as usize..(g.start + g.len) as usize)?;
            let first = self.routings.get(*members.first()? as usize)?;
            Some((&first.target, TargetRoutings { routings: self.routings, members }))
        })
    }

    /// `target` を指す routing (無ければ空)。
    #[must_use]
    pub fn routings_for(self, target: &AutomationTarget) -> TargetRoutings<'a> {
        self.targets_of(ParamSubject::of(target))
            .find(|(t, _)| *t == target)
            .map_or_else(TargetRoutings::default, |(_, r)| r)
    }
}
