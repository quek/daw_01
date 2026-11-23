//! r.md #158: トラックインスペクタの帯と「詳細」の開閉状態 (`docs/plan_rmd_158_inspector.md` §2.5)。
//!
//! **見方の都合** なので Song に載せず dirty も立てない (`ProjectView.inspector_folds`、`ViewState` に
//! id の文字列で保存)。鍵は **種類だけ** (トラック id / クリップ id を持たない) — 音声クリップの帯を
//! 畳めば、どの音声クリップを選んでも畳まれたまま、トラックの帯は全トラック共通 (仕様 6)。
//!
//! 保存を enum ではなく文字列 id にするのは、新しい版が足した種類を古い版が読んでも
//! `ViewState` 全体の読み込みを失敗させないため (知らない id は捨てる。`SettingsCategory` と同じ形)。

use std::collections::BTreeSet;

/// クリップの帯の種類 (`ClipContent` の variant と 1 対 1)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ClipBandKind {
    Audio,
    Midi,
    Image,
    Video,
    Text,
    Automation,
}

impl ClipBandKind {
    /// クリップの中身の種類。
    #[must_use]
    pub fn of(content: &common::model::ClipContent) -> Self {
        use common::model::ClipContent as C;
        match content {
            C::Audio(_) => Self::Audio,
            C::Midi(_) => Self::Midi,
            C::Image(_) => Self::Image,
            C::Video(_) => Self::Video,
            C::Text(_) => Self::Text,
            C::Automation(_) => Self::Automation,
        }
    }
}

/// インスペクタの最上位の帯 (見出しのクリックで開閉する単位)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InspectorBand {
    /// 選んでいるクリップ (アレンジ / ランチャーのセル)。
    Clip(ClipBandKind),
    /// 選んでいる列 (シーン) のフォローアクション。
    Scene,
    Input,
    Rack,
    MouthMap,
    Modulation,
}

/// 帯の中の「詳細」(めったに使わない欄、最初は畳む)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InspectorDetail {
    /// ランチャーの MIDI 割り当て (セルと列で共通)。
    LaunchMidi,
    /// 字幕の細かい設定 (塗り / 縁 / 影 / 回転)。
    SubtitleStyle,
}

/// 保存用の id 表 (書く側と読む側が同じ表を引く)。
const BAND_IDS: [(InspectorBand, &str); 11] = [
    (InspectorBand::Clip(ClipBandKind::Audio), "clip.audio"),
    (InspectorBand::Clip(ClipBandKind::Midi), "clip.midi"),
    (InspectorBand::Clip(ClipBandKind::Image), "clip.image"),
    (InspectorBand::Clip(ClipBandKind::Video), "clip.video"),
    (InspectorBand::Clip(ClipBandKind::Text), "clip.text"),
    (InspectorBand::Clip(ClipBandKind::Automation), "clip.automation"),
    (InspectorBand::Scene, "scene"),
    (InspectorBand::Input, "track.input"),
    (InspectorBand::Rack, "track.rack"),
    (InspectorBand::MouthMap, "track.mouth"),
    (InspectorBand::Modulation, "track.modulation"),
];

const DETAIL_IDS: [(InspectorDetail, &str); 2] = [
    (InspectorDetail::LaunchMidi, "detail.launch_midi"),
    (InspectorDetail::SubtitleStyle, "detail.subtitle_style"),
];

impl InspectorBand {
    /// 保存用の id。
    #[must_use]
    pub fn id(self) -> &'static str {
        BAND_IDS.iter().find(|(b, _)| *b == self).map_or("", |(_, id)| id)
    }

    /// 保存された id から (知らない id は `None` = 捨てる)。
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        BAND_IDS.iter().find(|(_, s)| *s == id).map(|(b, _)| *b)
    }
}

impl InspectorDetail {
    /// 保存用の id。
    #[must_use]
    pub fn id(self) -> &'static str {
        DETAIL_IDS.iter().find(|(d, _)| *d == self).map_or("", |(_, id)| id)
    }

    /// 保存された id から (知らない id は `None` = 捨てる)。
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        DETAIL_IDS.iter().find(|(_, s)| *s == id).map(|(d, _)| *d)
    }
}

/// 帯と詳細の開閉状態。既定は「帯は全部開・詳細は全部閉」で、既定と違うものだけ持つ。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InspectorFolds {
    /// 畳んでいる帯 (既定は開)。
    pub collapsed: BTreeSet<InspectorBand>,
    /// 開いている詳細 (既定は閉)。
    pub open_details: BTreeSet<InspectorDetail>,
}

impl InspectorFolds {
    #[must_use]
    pub fn band_open(&self, band: InspectorBand) -> bool {
        !self.collapsed.contains(&band)
    }

    #[must_use]
    pub fn detail_open(&self, detail: InspectorDetail) -> bool {
        self.open_details.contains(&detail)
    }

    /// 帯を開閉する。返り値 = 開閉後に開いているか。
    pub fn toggle_band(&mut self, band: InspectorBand) -> bool {
        if self.collapsed.remove(&band) {
            true
        } else {
            self.collapsed.insert(band);
            false
        }
    }

    /// 詳細を開閉する。返り値 = 開閉後に開いているか。
    pub fn toggle_detail(&mut self, detail: InspectorDetail) -> bool {
        if self.open_details.remove(&detail) {
            false
        } else {
            self.open_details.insert(detail);
            true
        }
    }

    /// 保存用: 畳んでいる帯の id (並びは `BTreeSet` の順 = save 差分が安定)。
    #[must_use]
    pub fn collapsed_ids(&self) -> Vec<String> {
        self.collapsed.iter().map(|b| b.id().to_string()).collect()
    }

    /// 保存用: 開いている詳細の id。
    #[must_use]
    pub fn open_detail_ids(&self) -> Vec<String> {
        self.open_details.iter().map(|d| d.id().to_string()).collect()
    }

    /// 保存された id から組み直す (知らない id は捨てる)。
    #[must_use]
    pub fn from_ids(collapsed: &[String], open_details: &[String]) -> Self {
        Self {
            collapsed: collapsed.iter().filter_map(|s| InspectorBand::from_id(s)).collect(),
            open_details: open_details.iter().filter_map(|s| InspectorDetail::from_id(s)).collect(),
        }
    }
}

/// インスペクタが「いま何を見せているか」(スクロールの床を持ち越してよい範囲)。カーソルのトラック・
/// 選んでいるトラック・クリップ・列のどれかが変われば別の主語。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InspectorSubject {
    pub cursor_track: Option<u32>,
    pub tracks: Vec<u32>,
    pub clips: Vec<common::model::ClipKey>,
    pub scenes: Vec<u32>,
}

/// r.md #158 §6-1: 本文高の床 (`ProjectEphemeral.inspector_scroll_floor`)。
#[derive(Debug, Clone, PartialEq)]
pub struct InspectorScrollFloor {
    /// 前フレームの主語。
    pub subject: InspectorSubject,
    /// 本文高の下限 (px、本文の起点から)。`None` = 床なし。
    pub floor: Option<f32>,
}

impl InspectorScrollFloor {
    /// 今フレームの主語で使ってよい床 (主語が変わっていれば無い)。
    #[must_use]
    pub fn floor_for(this: Option<&Self>, subject: &InspectorSubject) -> Option<f32> {
        this.filter(|f| f.subject == *subject).and_then(|f| f.floor)
    }

    /// 描き終えたフレームから次の床を決める。`measured` = 今フレームの本文の実高、
    /// `bottom` = scroll offset + viewport 高 (いま見えている下端、本文の起点から)。
    ///
    /// - 主語が前フレームと違えば床を持たない (別の中身を古い offset のまま見せない)。
    /// - 本文が見えている下端より短ければ、下端を床にする (縮んだ瞬間に offset を詰めさせない)。
    ///   上へスクロールすると下端と一緒に床も下がり、本文に追いついたら消える。
    #[must_use]
    pub fn next(this: Option<&Self>, subject: InspectorSubject, measured: f32, bottom: f32) -> Self {
        let same = this.is_some_and(|f| f.subject == subject);
        let prev = Self::floor_for(this, &subject);
        let floor = (same && measured + 0.5 < bottom).then(|| prev.map_or(bottom, |p| p.min(bottom)));
        Self { subject, floor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 床: 末尾で本文が縮んでも見えている下端を保ち、上へスクロールすると下がって消え、
    /// 主語が変わったフレームは持ち越さない。
    #[test]
    fn inspector_scroll_floor_holds_the_bottom_until_scrolled_back() {
        let a = InspectorSubject { cursor_track: Some(1), ..InspectorSubject::default() };
        let b = InspectorSubject { cursor_track: Some(2), ..InspectorSubject::default() };
        // 本文 1000、viewport 500 で末尾 (offset 500 → 下端 1000)。
        let f = InspectorScrollFloor::next(None, a.clone(), 1000.0, 1000.0);
        assert_eq!(f.floor, None, "最初のフレームは主語が無い");
        // 帯を畳んで本文が 700 に縮む → 床 1000。
        let f = InspectorScrollFloor::next(Some(&f), a.clone(), 700.0, 1000.0);
        assert_eq!(InspectorScrollFloor::floor_for(Some(&f), &a), Some(1000.0));
        // 上へ 100 スクロール → 床 900。
        let f = InspectorScrollFloor::next(Some(&f), a.clone(), 700.0, 900.0);
        assert_eq!(f.floor, Some(900.0));
        // 本文に追いついたら消える。
        let g = InspectorScrollFloor::next(Some(&f), a.clone(), 700.0, 700.0);
        assert_eq!(g.floor, None);
        // 主語が変われば持ち越さず、そのフレームも床を作らない。
        let h = InspectorScrollFloor::next(Some(&f), b.clone(), 300.0, 900.0);
        assert_eq!(InspectorScrollFloor::floor_for(Some(&f), &b), None);
        assert_eq!(h.floor, None);
    }

    /// id 表は全 variant を 1 回ずつ持ち (書いた id が必ず読める)、知らない id は捨てる。
    #[test]
    fn inspector_fold_ids_round_trip_and_unknown_ids_are_dropped() {
        let mut ids: Vec<&str> = BAND_IDS.iter().map(|(_, s)| *s).collect();
        ids.extend(DETAIL_IDS.iter().map(|(_, s)| *s));
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "id が重複していない");
        for (band, id) in BAND_IDS {
            assert_eq!(InspectorBand::from_id(band.id()), Some(band), "{id}");
        }
        for (detail, id) in DETAIL_IDS {
            assert_eq!(InspectorDetail::from_id(detail.id()), Some(detail), "{id}");
        }
        let mut folds = InspectorFolds::default();
        folds.toggle_band(InspectorBand::Rack);
        folds.toggle_band(InspectorBand::Clip(ClipBandKind::Audio));
        folds.toggle_detail(InspectorDetail::LaunchMidi);
        let mut collapsed = folds.collapsed_ids();
        collapsed.push("clip.hologram".into());
        let mut details = folds.open_detail_ids();
        details.push("detail.future".into());
        assert_eq!(InspectorFolds::from_ids(&collapsed, &details), folds, "知らない id は捨てて残りは戻る");
    }
}
