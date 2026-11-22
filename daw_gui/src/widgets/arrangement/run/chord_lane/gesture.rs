//! 帯のドラッグの解決。**描画 (ドラッグ中のプレビュー) と release の確定がこの 1 本を通る**ので、
//! 見えていた位置と落ちる位置が食い違わない (`section_preview_start_len` と同じ構造)。
//!
//! 位置の snap は「掴んだ端の絶対位置を grid に乗せ、その差を全員に足す」 (ui/CLAUDE.md の
//! 絶対位置 snap)。Alt は snap を外す (他のドラッグと同じ)。

use super::{ChordBox, ChordLaneView};
use common::snap::SnapConfig;

/// 箱として残す最小の長さ (拍)。Arranger の帯と同じ下限 (`SECTION_MIN_LEN_BEATS`)。
pub(super) const MIN_LEN: f64 = 1.0 / 16.0;

/// 箱のどこを掴んだか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Grab {
    Move,
    Left,
    Right,
}

/// 端のドラッグの種類 (press で決まる)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum EdgeMode {
    /// 掴んだ箱だけ (かかった隣は上書き規則で削られる)。
    Single,
    /// 境界の下半分: 接している隣の箱 `(id, start, len)` も境界ごと動く (Studio One)。
    Linked((u32, f64, f64)),
    /// Alt: 選択全体を、掴んだ端の反対側の外端を軸に比例伸縮する。
    Proportional,
}

/// 箱のドラッグ 1 回。
#[derive(Clone, Debug)]
pub(super) struct ChordDrag {
    pub grab: Grab,
    /// 掴んだ箱と press 時の範囲。
    pub id: u32,
    pub anchor: (f64, f64),
    /// 一緒に動く箱 (Move / Proportional) の press 時の範囲 `(id, start, len)`。掴んだ箱を含む。
    pub targets: Vec<(u32, f64, f64)>,
    pub edge: EdgeMode,
    pub anchor_mouse: (f32, f32),
    pub last_mouse: (f32, f32),
    /// release フレームの modifier は ModifiersChanged の先行で落ちうるので、継続フレームの値を持つ。
    pub last_alt: bool,
    pub last_ctrl: bool,
    /// press 時の Shift / Ctrl (短クリックの選択の解決に使う)。
    pub press_shift: bool,
    pub press_ctrl: bool,
}

/// 空きのドラッグ (範囲で箱を選ぶ)。
#[derive(Clone, Copy, Debug)]
pub(super) struct RangeDrag {
    pub anchor_beat: f64,
    pub anchor_mouse: (f32, f32),
    pub last_mouse: (f32, f32),
    pub press_shift: bool,
    pub press_ctrl: bool,
}

/// ドラッグの解決結果。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Resolved {
    /// 箱をまとめて `delta` 拍ずらす (`duplicate` なら元を残して複製)。
    Move { ids: Vec<u32>, delta: f64, duplicate: bool },
    /// 箱の範囲を変える `(id, start, len)`。
    Spans(Vec<(u32, f64, f64)>),
}

/// snap と尺度 (ドラッグの解決に要るものだけ)。
#[derive(Clone, Copy)]
pub(super) struct Scale<'a> {
    pub beat_per_px: f64,
    pub snap: &'a SnapConfig,
    pub zoom: f32,
}

impl ChordDrag {
    fn raw_delta(&self, s: Scale<'_>) -> f64 {
        f64::from(self.last_mouse.0 - self.anchor_mouse.0) * s.beat_per_px
    }

    /// 掴んだ端の絶対位置を grid に乗せた差分。
    fn snapped_delta(&self, s: Scale<'_>) -> f64 {
        let raw = self.raw_delta(s);
        let pivot = match self.grab {
            Grab::Move | Grab::Left => self.anchor.0,
            Grab::Right => self.anchor.0 + self.anchor.1,
        };
        s.snap.snap_beat(pivot + raw, self.last_alt, s.zoom) - pivot
    }

    /// 動かしていない (= 短クリック) とみなすか。端は常に確定する (ui/CLAUDE.md の閾値の規則)。
    pub fn is_click(&self) -> bool {
        let d = (self.last_mouse.0 - self.anchor_mouse.0).abs() + (self.last_mouse.1 - self.anchor_mouse.1).abs();
        self.grab == Grab::Move && !self.last_alt && d < 4.0
    }

    pub fn resolve(&self, s: Scale<'_>) -> Resolved {
        match self.grab {
            Grab::Move => {
                let min_start = self.targets.iter().map(|t| t.1).fold(f64::INFINITY, f64::min);
                // `Song::move_chords` と同じく左端が 0 を割らないよう詰める (複製も同じ位置に見せる)。
                let delta = self.snapped_delta(s).max(-min_start);
                Resolved::Move {
                    ids: self.targets.iter().map(|t| t.0).collect(),
                    delta,
                    duplicate: self.last_ctrl,
                }
            }
            Grab::Left | Grab::Right => Resolved::Spans(self.resolve_edge(s)),
        }
    }

    fn resolve_edge(&self, s: Scale<'_>) -> Vec<(u32, f64, f64)> {
        let (start, len) = self.anchor;
        let end = start + len;
        match (self.edge, self.grab) {
            (EdgeMode::Proportional, grab) => proportional(&self.targets, grab, self.raw_delta(s)),
            (EdgeMode::Linked((nid, ns, nl)), Grab::Right) => {
                let b = (end + self.snapped_delta(s)).clamp(start + MIN_LEN, (ns + nl - MIN_LEN).max(start + MIN_LEN));
                vec![(self.id, start, b - start), (nid, b, ns + nl - b)]
            }
            (EdgeMode::Linked((nid, ns, _)), Grab::Left) => {
                let b = (start + self.snapped_delta(s)).clamp(ns + MIN_LEN, (end - MIN_LEN).max(ns + MIN_LEN));
                vec![(nid, ns, b - ns), (self.id, b, end - b)]
            }
            (_, Grab::Left) => {
                let ns = (start + self.snapped_delta(s)).clamp(0.0, (end - MIN_LEN).max(0.0));
                vec![(self.id, ns, end - ns)]
            }
            (_, _) => vec![(self.id, start, (len + self.snapped_delta(s)).max(MIN_LEN))],
        }
    }
}

/// Alt の比例伸縮。右端を掴めば選択の左端が、左端を掴めば選択の右端が動かない軸になる。
fn proportional(targets: &[(u32, f64, f64)], grab: Grab, raw: f64) -> Vec<(u32, f64, f64)> {
    let lo = targets.iter().map(|t| t.1).fold(f64::INFINITY, f64::min);
    let hi = targets.iter().map(|t| t.1 + t.2).fold(f64::NEG_INFINITY, f64::max);
    let extent = hi - lo;
    if extent <= 0.0 {
        return Vec::new();
    }
    let shortest = targets.iter().map(|t| t.2).fold(f64::INFINITY, f64::min);
    // 一番短い箱が MIN_LEN を割らない、左へ伸ばしても 0 を割らない倍率に詰める。
    let min_scale = MIN_LEN / shortest;
    match grab {
        Grab::Right => {
            let scale = ((extent + raw) / extent).max(min_scale);
            targets.iter().map(|&(id, st, l)| (id, lo + (st - lo) * scale, l * scale)).collect()
        }
        _ => {
            let scale = ((extent - raw) / extent).max(min_scale).min(hi / extent);
            targets.iter().map(|&(id, st, l)| (id, hi - (hi - st) * scale, l * scale)).collect()
        }
    }
}

impl RangeDrag {
    /// 範囲 `[a, b)` (拍、snap なし — 箱を選ぶだけなので grid に乗せない)。
    pub fn span(&self, s: Scale<'_>) -> (f64, f64) {
        let cur = self.anchor_beat + f64::from(self.last_mouse.0 - self.anchor_mouse.0) * s.beat_per_px;
        (self.anchor_beat.min(cur), self.anchor_beat.max(cur))
    }

    pub fn is_click(&self) -> bool {
        (self.last_mouse.0 - self.anchor_mouse.0).abs() < 4.0
    }

    /// 範囲にかかる箱。
    pub fn hits(&self, view: &ChordLaneView, s: Scale<'_>) -> Vec<u32> {
        let (a, b) = self.span(s);
        view.boxes.iter().filter(|c| c.start < b && c.start + c.len > a).map(|c| c.id).collect()
    }
}

/// press 時に決める端のドラッグの種類。
pub(super) fn edge_mode(
    view: &ChordLaneView,
    grabbed: &ChordBox,
    grab: Grab,
    lower_half: bool,
    alt: bool,
    selected: &[u32],
) -> EdgeMode {
    if alt && selected.len() > 1 && selected.contains(&grabbed.id) {
        return EdgeMode::Proportional;
    }
    if !lower_half {
        return EdgeMode::Single;
    }
    const EPS: f64 = 1e-9;
    let neighbor = match grab {
        Grab::Right => view.boxes.iter().find(|c| (c.start - (grabbed.start + grabbed.len)).abs() < EPS),
        Grab::Left => view.boxes.iter().find(|c| (c.start + c.len - grabbed.start).abs() < EPS),
        Grab::Move => None,
    };
    neighbor.map_or(EdgeMode::Single, |n| EdgeMode::Linked((n.id, n.start, n.len)))
}
