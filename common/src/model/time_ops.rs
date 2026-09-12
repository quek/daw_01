//! 時間範囲操作 — Live §6.11 の "…Time" コマンド群 (Cut / Paste / Duplicate / Delete Time /
//! Insert Silence) の Song 側プリミティブ。**全トラック縦断**で時間そのものを削除 / 挿入 /
//! 複製し、以降を ripple で詰める / 押し出す (`docs/plan_time_ops.md`)。
//!
//! Arranger セクションの範囲削除 / 複製 (`delete_section_range` / `duplicate_section`) も
//! ここの [`Song::delete_time_range`] / [`Song::paste_time_range`] を通る (時間を動かす
//! 規則は 1 本)。
//!
//! セクション帯の規則 (Studio One 流):
//! - 削除範囲に完全に入る帯は消え、範囲をまたぐ帯は重なりぶん縮む。
//! - 挿入点をまたぐ帯は挿入ぶん伸びる (帯の中に時間を差し込んだ = その帯が長くなる)。
//! - 複製 / 貼り付けで運ぶ帯は、範囲に完全に入っていたものだけ (新 id で置く)。

use super::{MediaManifest, MediaRemap};
use super::*;

/// 時間範囲 `[a, b)` の中身の写し。Cut Time / Paste Time が clipboard で運ぶ単位で、
/// Duplicate Time はプロセス内でそのまま貼る。位置はすべて **範囲の先頭 `a` からの相対**。
///
/// 貼り先は同じプロジェクトを前提にトラック id / レーン id で宛先を引く (無いものは
/// 落とす)。content は `contents` に写しを持ち、貼り先に同じ content が現存すれば
/// 共有 (linked)、無ければ写しから作る (`Song::alloc_content`)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeRangeCopy {
    /// 範囲の長さ (拍)。貼るときに差し込む時間の長さ。
    pub span_beats: f64,
    /// `(track id, 範囲に詰めた窓)`。
    pub clips: Vec<(u32, Clip)>,
    /// `(track id — `MASTER_TRACK_ID` は `song_lanes`, lane id, 範囲に詰めた窓)`。
    pub automation: Vec<(u32, u32, AutomationClip)>,
    /// 参照している content の写し (`content_id` → (content, 共有名))。
    pub contents: HashMap<ContentId, (ClipContent, String)>,
    /// content が参照する媒体 (音源 / 映像 / 画像) の写し。別プロジェクトへ貼るとき
    /// `Song::import_media` で取り込む (無いと `source_id` が宙に浮いて殻だけ貼られる)。
    #[serde(default, skip_serializing_if = "MediaManifest::is_empty")]
    pub media: MediaManifest,
    pub scale_changes: Vec<ScaleChange>,
    /// 範囲に完全に入っていたセクション帯 (`id` は貼り先で採番し直す)。
    pub sections: Vec<Section>,
}

impl TimeRangeCopy {
    /// OS clipboard から来た写し (= 外部入力) の値域を検証する。 非有限 / 負 / 長さ 0 の
    /// 位置を持つ要素は落とし、 `span_beats` が非有限か 0 以下なら `None`。
    /// 同じプロセス内 (Duplicate Time) の写しは通す必要が無い — clipboard 経由だけ。
    #[must_use]
    pub fn sanitized(mut self) -> Option<Self> {
        if !self.span_beats.is_finite() || self.span_beats <= 0.0 {
            return None;
        }
        let ok = |start: f64, len: f64, off: f64| {
            start.is_finite() && start >= 0.0 && len.is_finite() && len > 0.0 && off.is_finite()
        };
        self.clips
            .retain(|(_, c)| ok(c.start_beat, c.length_beats, c.content_offset_beats));
        self.automation
            .retain(|(_, _, c)| ok(c.start_beat, c.length_beats, c.content_offset_beats));
        self.scale_changes.retain(|sc| sc.beat.is_finite() && sc.beat >= 0.0);
        self.sections.retain(|s| ok(s.start_beat, s.len_beats, 0.0));
        Some(self)
    }
}

/// [`Song::paste_time_range`] の結果。
#[derive(Debug, Clone, PartialEq)]
pub struct PastedTime {
    /// 差し込みで適用した ripple (Song の外の時間位置の追従用)。
    pub ripple: Ripple,
    /// 貼ったセクション帯の新 id (`copy.sections` と同順)。
    pub section_ids: Vec<u32>,
}

impl Song {
    /// `[a, b)` の中身を写す (Song は変えない)。範囲からはみ出すクリップは窓を範囲に
    /// 詰める (content は触らない)。`b <= a` なら `None`。
    #[must_use]
    pub fn copy_time_range(&self, a: f64, b: f64) -> Option<TimeRangeCopy> {
        if !a.is_finite() || !b.is_finite() || b <= a {
            return None;
        }
        let mut out = TimeRangeCopy { span_beats: b - a, ..Default::default() };
        let mut need: Vec<ContentId> = Vec::new();
        for t in &self.tracks {
            for c in &t.clips {
                let (s, e) = c.song_window();
                let (cs, ce) = (s.max(a), e.min(b));
                if ce <= cs + f64::EPSILON {
                    continue;
                }
                let mut cropped = c.clone();
                cropped.content_offset_beats += cs - s;
                cropped.start_beat = cs - a;
                cropped.length_beats = ce - cs;
                // 新しい窓にクロスフェードの張り出しは無い (範囲の端で切れる)。
                cropped.xfade_lead_beats = 0.0;
                cropped.xfade_tail_beats = 0.0;
                need.push(cropped.content_id);
                out.clips.push((t.id, cropped));
            }
            for lane in &t.automation_lanes {
                crop_automation_lane(lane, t.id, a, b, &mut out, &mut need);
            }
        }
        for lane in &self.song_lanes {
            crop_automation_lane(lane, MASTER_TRACK_ID, a, b, &mut out, &mut need);
        }
        for cid in need {
            if out.contents.contains_key(&cid) {
                continue;
            }
            if let Some(content) = self.clip_contents.get(&cid) {
                let name = self.clip_content_names.get(&cid).cloned().unwrap_or_default();
                out.contents.insert(cid, (content.clone(), name));
            }
        }
        out.media = self.media_manifest_for(out.contents.values().map(|(c, _)| c));
        out.scale_changes = self
            .scale_changes
            .iter()
            .filter(|sc| sc.beat >= a && sc.beat < b)
            .map(|sc| ScaleChange { beat: sc.beat - a, ..*sc })
            .collect();
        out.sections = self
            .sections
            .iter()
            .filter(|s| s.start_beat >= a - f64::EPSILON && s.start_beat + s.len_beats <= b + f64::EPSILON)
            .map(|s| Section { start_beat: (s.start_beat - a).max(0.0), ..s.clone() })
            .collect();
        Some(out)
    }

    /// `[a, b)` の時間を全トラックから取り除いて詰める (Live の Delete Time / Cut Time)。
    /// 境界で窓を割ってから範囲内の全 content を消し、`b` 以降を `-(b - a)` ripple する。
    /// セクション帯は完全に入るものが消え、またぐものが重なりぶん縮む。
    /// 何もしなかったら `None`。
    pub fn delete_time_range(&mut self, a: f64, b: f64) -> Option<Ripple> {
        if !a.is_finite() || !b.is_finite() || b <= a {
            return None;
        }
        let len = b - a;
        self.split_clips_at(a);
        self.split_clips_at(b);
        let in_range = |s: f64| s >= a && s < b;
        for t in &mut self.tracks {
            t.clips.retain(|c| !in_range(c.start_beat));
            for lane in &mut t.automation_lanes {
                lane.clips.retain(|c| !in_range(c.start_beat));
            }
        }
        for lane in &mut self.song_lanes {
            lane.clips.retain(|c| !in_range(c.start_beat));
        }
        self.scale_changes.retain(|sc| !in_range(sc.beat));
        // セクション帯: 範囲との重なりを引く (完全に入る帯は長さ 0 → 消える)。
        for s in &mut self.sections {
            let (s0, s1) = (s.start_beat, s.start_beat + s.len_beats);
            let n0 = if s0 < a { s0 } else { (s0 - len).max(a) };
            let n1 = if s1 <= a {
                s1
            } else if s1 <= b {
                a
            } else {
                s1 - len
            };
            s.start_beat = n0;
            s.len_beats = (n1 - n0).max(0.0);
        }
        self.sections.retain(|s| s.len_beats > f64::EPSILON);
        let close = self.ripple_timeline_with(b, -len, false);
        self.ensure_scale_changes_sorted();
        self.normalize_sections();
        Some(close)
    }

    /// `at` に `len` 拍の空き時間を差し込む (Live の Insert Silence / 貼り付けの前段)。
    /// `at` をまたぐ窓は割り、以降を `+len` ripple する。`at` をまたぐセクション帯は
    /// `len` ぶん伸びる。`len <= 0` なら `None`。
    pub fn insert_time(&mut self, at: f64, len: f64) -> Option<Ripple> {
        if !len.is_finite() || len <= 0.0 || !at.is_finite() || at < 0.0 {
            return None;
        }
        self.split_clips_at(at);
        for s in &mut self.sections {
            if s.start_beat < at && at < s.start_beat + s.len_beats {
                s.len_beats += len;
            }
        }
        let open = self.ripple_timeline(at, len);
        self.normalize_sections();
        Some(open)
    }

    /// `copy` を `at` に**時間ごと**貼る (Live の Paste Time)。`copy.span_beats` の空き時間を
    /// 差し込んでから中身を置く。`same_project` なら現存する content を共有 (linked)、
    /// そうでなければ写しから作る。宛先のトラック / レーンが無い中身は落とす。
    pub fn paste_time_range(
        &mut self,
        at: f64,
        copy: &TimeRangeCopy,
        same_project: bool,
    ) -> Option<PastedTime> {
        let ripple = self.insert_time(at, copy.span_beats)?;
        // 別プロジェクトからなら媒体を先に取り込む (content の source_id を張り替える)。
        let media_remap =
            if same_project { MediaRemap::default() } else { self.import_media(&copy.media) };
        let mut remap: HashMap<ContentId, ContentId> = HashMap::new();
        let mut resolve = |song: &mut Song, cid: ContentId| -> Option<ContentId> {
            if let Some(&new) = remap.get(&cid) {
                return Some(new);
            }
            let new = if same_project && song.clip_contents.contains_key(&cid) {
                cid
            } else if let Some((content, name)) = copy.contents.get(&cid) {
                let mut content = content.clone();
                content.remap_media(&media_remap);
                song.alloc_content(content, name.clone())
            } else if same_project {
                // 写しにも現物にも無い (未採番 content の手組み Song 等)。 同じプロジェクト
                // なら id をそのまま共有する (旧 `duplicate_section` と同じ)。
                cid
            } else {
                return None;
            };
            remap.insert(cid, new);
            Some(new)
        };
        for (tid, c) in &copy.clips {
            let Some(content_id) = resolve(self, c.content_id) else { continue };
            let Some(t) = self.tracks.iter_mut().find(|t| t.id == *tid) else { continue };
            t.place_clip(Clip {
                id: 0,
                start_beat: at + c.start_beat,
                content_id,
                ..c.clone()
            });
        }
        for (tid, lid, c) in &copy.automation {
            let Some(content_id) = resolve(self, c.content_id) else { continue };
            let Some(lane) = self.automation_lane_by_key_mut(*tid, *lid) else { continue };
            let id = lane.alloc_clip_id();
            lane.clips.push(AutomationClip {
                id,
                start_beat: at + c.start_beat,
                content_id,
                ..c.clone()
            });
        }
        for sc in &copy.scale_changes {
            self.scale_changes.push(ScaleChange { beat: sc.beat + at, ..*sc });
        }
        let mut section_ids = Vec::with_capacity(copy.sections.len());
        for s in &copy.sections {
            let id = self.alloc_section_id();
            section_ids.push(id);
            self.sections.push(Section { id, start_beat: s.start_beat + at, ..s.clone() });
        }
        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        self.normalize_sections();
        Some(PastedTime { ripple, section_ids })
    }

    /// `[a, b)` を直後 (`b`) に時間ごと複製する (Live の Duplicate Time)。
    pub fn duplicate_time_range(&mut self, a: f64, b: f64) -> Option<Ripple> {
        let copy = self.copy_time_range(a, b)?;
        self.paste_time_range(b, &copy, true).map(|p| p.ripple)
    }
}

/// レーンのクリップを `[a, b)` に詰めて `out.automation` へ積む ([`Song::copy_time_range`])。
fn crop_automation_lane(
    lane: &AutomationLane,
    track_id: u32,
    a: f64,
    b: f64,
    out: &mut TimeRangeCopy,
    need: &mut Vec<ContentId>,
) {
    for c in &lane.clips {
        let (s, e) = (c.start_beat, c.start_beat + c.length_beats);
        let (cs, ce) = (s.max(a), e.min(b));
        if ce <= cs + f64::EPSILON {
            continue;
        }
        let mut cropped = c.clone();
        cropped.content_offset_beats += cs - s;
        cropped.start_beat = cs - a;
        cropped.length_beats = ce - cs;
        need.push(cropped.content_id);
        out.automation.push((track_id, lane.id, cropped));
    }
}
