//! content の**分割** — `at` を跨ぐ note / event を 2 つに割る (copy-on-write)。
//!
//! 窓 (クリップ) の分割だけでは「跨いだ note の後半」 を後半の窓が鳴らせない
//! (再生側は「発音開始が窓内」 の note しか鳴らさない) ので、切り口では content 側も
//! 切る。 共有されている content は先に fork するので linked clip は無傷。
//!
//! 呼ぶのは分割を伴う 3 経路 — クリップの split (`E`) / セクションの境界
//! (`Song::split_clips_at`) / 範囲操作の両端 (`handler::range_ops`)。

use crate::model::{
    AudioEvent, ClipContent, ContentId, Note, Song, VideoEvent,
};

impl Song {
    /// content を **content-local 拍 `at`** で切る。 `at` を跨ぐ note / event を 2 つに割り、
    /// 切り口 (内側) の fade は 0 にする。 **位置は動かさない** — 窓モデルなので、
    /// 切った 1 つの content の上に 2 つの窓 (= 分割後の 2 クリップ) が乗る。
    ///
    /// content が複数の clip から共有されていれば**先に fork する** (copy-on-write) ので、
    /// linked clip の中身は変わらない。 返り値は「切り終えた content の id」= 分割後の
    /// 両断片が使う id。 跨ぐ要素が無ければ fork もせず `content_id` をそのまま返す。
    ///
    /// これが無いと、 窓を割っただけでは**後半の窓が「跨いだ note の後半」を鳴らせない**
    /// (再生側は「発音開始が窓内」の note しか鳴らさないため)。
    pub fn split_content_at(&mut self, content_id: ContentId, at: f64) -> ContentId {
        let crosses = self
            .clip_contents
            .get(&content_id)
            .is_some_and(|c| Self::content_crosses(c, at));
        if !crosses {
            return content_id;
        }
        let target = if self.clip_content_refcount(content_id) > 1 {
            self.fork_content(content_id)
        } else {
            content_id
        };
        if let Some(content) = self.clip_contents.get_mut(&target) {
            Self::cut_content_at(content, at);
            content.ensure_element_ids();
        }
        target
    }

    /// `at` を厳密に跨ぐ要素があるか (`split_content_at` の早期 return 判定)。
    fn content_crosses(content: &ClipContent, at: f64) -> bool {
        const EPS: f64 = 1e-9;
        let crosses = |start: f64, len: f64| start < at - EPS && start + len > at + EPS;
        match content {
            ClipContent::Midi(m) => {
                m.notes.iter().any(|n| crosses(n.start_beat, n.duration_beats))
            }
            ClipContent::Audio(a) => a
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Video(v) => v
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Image(i) => i
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Text(t) => t
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            // automation point は幅を持たないので切る対象が無い (窓の外の point も
            // 補間には効くので、境界に point を挿す必要も無い)。
            ClipContent::Automation(_) => false,
        }
    }

    /// `at` を跨ぐ要素を 2 つに割る (位置は動かさない)。
    fn cut_content_at(content: &mut ClipContent, at: f64) {
        const EPS: f64 = 1e-9;
        /// 時間軸 source を持たない overlay (image / text) 用。
        macro_rules! cut_overlay {
            ($events:expr) => {{
                let mut extra = Vec::new();
                for ev in $events.iter_mut() {
                    let e0 = ev.event_start_in_clip_beats;
                    let e1 = e0 + ev.event_length_beats;
                    if !(e0 < at - EPS && e1 > at + EPS) {
                        continue;
                    }
                    let mut tail = ev.clone();
                    tail.event_start_in_clip_beats = at;
                    tail.event_length_beats = e1 - at;
                    tail.fade_in_beats = 0.0;
                    extra.push(tail);
                    ev.event_length_beats = at - e0;
                    ev.fade_out_beats = 0.0;
                }
                $events.extend(extra);
                $events.sort_by(|a, b| {
                    a.event_start_in_clip_beats.total_cmp(&b.event_start_in_clip_beats)
                });
            }};
        }
        match content {
            ClipContent::Midi(m) => {
                let mut extra: Vec<Note> = Vec::new();
                for n in &mut m.notes {
                    let end = n.start_beat + n.duration_beats;
                    if !(n.start_beat < at - EPS && end > at + EPS) {
                        continue;
                    }
                    let mut tail = n.clone();
                    tail.id = 0;
                    tail.start_beat = at;
                    tail.duration_beats = end - at;
                    // 後半は継続なので歌詞を持たない (VOICEVOX が二重に歌わない)。
                    tail.lyric = None;
                    extra.push(tail);
                    n.duration_beats = at - n.start_beat;
                }
                m.notes.extend(extra);
                m.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
            }
            ClipContent::Audio(a) => {
                let mut extra: Vec<AudioEvent> = Vec::new();
                for ev in &mut a.events {
                    let e0 = ev.event_start_in_clip_beats;
                    let e1 = e0 + ev.event_length_beats;
                    if !(e0 < at - EPS && e1 > at + EPS) {
                        continue;
                    }
                    let frac = (at - e0) / ev.event_length_beats;
                    let span = ev.source_end_frames.saturating_sub(ev.source_start_frames);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
                    let delta = (span as f64 * frac).round().max(0.0) as u64;
                    let mut tail = ev.clone();
                    tail.id = 0;
                    tail.event_start_in_clip_beats = at;
                    tail.event_length_beats = e1 - at;
                    tail.fade_in_beats = 0.0;
                    ev.event_length_beats = at - e0;
                    ev.fade_out_beats = 0.0;
                    if ev.reversed {
                        // 逆再生: 前半は source の**末尾**側を使う。
                        let mid = ev.source_end_frames.saturating_sub(delta);
                        tail.source_end_frames = mid.max(ev.source_start_frames);
                        ev.source_start_frames = mid.min(ev.source_end_frames);
                    } else {
                        let mid = ev.source_start_frames.saturating_add(delta);
                        tail.source_start_frames = mid.min(ev.source_end_frames);
                        ev.source_end_frames = mid.max(ev.source_start_frames);
                    }
                    extra.push(tail);
                }
                a.events.extend(extra);
                a.events.sort_by(|x, y| {
                    x.event_start_in_clip_beats.total_cmp(&y.event_start_in_clip_beats)
                });
            }
            ClipContent::Video(v) => {
                let mut extra: Vec<VideoEvent> = Vec::new();
                for ev in &mut v.events {
                    let e0 = ev.event_start_in_clip_beats;
                    let e1 = e0 + ev.event_length_beats;
                    if !(e0 < at - EPS && e1 > at + EPS) {
                        continue;
                    }
                    let frac = (at - e0) / ev.event_length_beats;
                    let span = ev.source_end_micros.saturating_sub(ev.source_start_micros);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
                    let delta = (span as f64 * frac).round().max(0.0) as u64;
                    let mid = ev.source_start_micros.saturating_add(delta);
                    let mut tail = ev.clone();
                    tail.event_start_in_clip_beats = at;
                    tail.event_length_beats = e1 - at;
                    tail.source_start_micros = mid.min(ev.source_end_micros);
                    tail.fade_in_beats = 0.0;
                    extra.push(tail);
                    ev.event_length_beats = at - e0;
                    ev.source_end_micros = mid.max(ev.source_start_micros);
                    ev.fade_out_beats = 0.0;
                }
                v.events.extend(extra);
                v.events.sort_by(|x, y| {
                    x.event_start_in_clip_beats.total_cmp(&y.event_start_in_clip_beats)
                });
            }
            ClipContent::Image(i) => cut_overlay!(i.events),
            ClipContent::Text(t) => cut_overlay!(t.events),
            ClipContent::Automation(_) => {}
        }
    }
}
