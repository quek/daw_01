//! **クリップの窓に見えている片への編集** — Inspector の値 / Auto-Fade / Auto-Crossfade の効く範囲の SSoT
//! (r.md #132 残件、2026-09-15 決定)。
//!
//! クリップは content の窓 (`docs/plan_clip_content_window.md`)。 分割の片は同じ content を別の窓で見るので、
//! クリップへの編集を content の全 event に掛けると、content を共有する反対側のクリップまで変わる。 編集は
//! **そのクリップの窓に見えている片だけ**に掛ける ([`shown_indices`])。
//!
//! 窓の中でひと続きになっている片 (同じ take の連続 = [`super::event_window::joinable`]) は 1 つの event と
//! みなす: つないで ([`joined_run`]) 編集し、元の片の切り口で切り直す ([`edit_run`])。 fade はひと続きの
//! 外側の端に付き、移調 / 逆再生 / 伸縮 mode はひと続き全体を 1 つの take として掛かるので、**分割直後に
//! 掛けても、分割前に掛けてから分割したのと同じ**になる。 表示もひと続きから読む ([`run_fade`])。

use super::event_window::{event_piece, join_into, joinable};
use super::{ClipContent, ClipKey, EventFade, Song, TimedEvent};

/// 拍の同一視許容量 ([`super::split_boundaries`] と同じ)。
const EPS: f64 = 1e-9;

/// `ev` が窓 `(lo, hi)` (content-local 拍) に正の長さで見えているか。
#[must_use]
pub fn shown_in<E: TimedEvent>(ev: &E, (lo, hi): (f64, f64)) -> bool {
    ev.start() < hi && ev.start() + ev.len() > lo
}

/// 窓 `window` に見えている event の index (開始拍順)。
#[must_use]
pub fn shown_indices<E: TimedEvent>(events: &[E], window: (f64, f64)) -> Vec<usize> {
    let targets: Vec<usize> = (0..events.len()).filter(|&i| shown_in(&events[i], window)).collect();
    sorted_by_start(events, &targets)
}

/// `targets` (順不同・重複可、範囲外は捨てる) を開始拍順にする。
fn sorted_by_start<E: TimedEvent>(events: &[E], targets: &[usize]) -> Vec<usize> {
    let mut sorted: Vec<usize> = targets.iter().copied().filter(|&i| i < events.len()).collect();
    sorted.sort_by(|&a, &b| events[a].start().total_cmp(&events[b].start()).then(a.cmp(&b)));
    sorted.dedup();
    sorted
}

/// `targets` をひと続きの片ごとに束ねる (run、各 run は開始拍順の index)。
#[must_use]
pub fn piece_runs<E: TimedEvent>(events: &[E], targets: &[usize]) -> Vec<Vec<usize>> {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for i in sorted_by_start(events, targets) {
        match runs.last_mut() {
            Some(run) if run.last().is_some_and(|&last| joinable(&events[last], &events[i])) => run.push(i),
            _ => runs.push(vec![i]),
        }
    }
    runs
}

/// run の片をつないだ 1 つの event。
#[must_use]
pub fn joined_run<E: TimedEvent>(events: &[E], run: &[usize]) -> Option<E> {
    let (&first, rest) = run.split_first()?;
    let mut joined = events.get(first)?.clone();
    for &i in rest {
        join_into(&mut joined, events.get(i)?);
    }
    Some(joined)
}

/// run をつないだ event の位置・長さ・両端の fade ([`joined_run`] の fade を、event を複製せずに読む口)。
#[must_use]
pub fn run_fade<E: TimedEvent>(events: &[E], run: &[usize]) -> Option<EventFade> {
    let first = events.get(*run.first()?)?.fade();
    let last = events.get(*run.last()?)?.fade();
    Some(EventFade {
        len_beats: last.start_in_clip_beats + last.len_beats - first.start_in_clip_beats,
        fade_out_beats: last.fade_out_beats,
        fade_out_curve: last.fade_out_curve,
        fade_out_trail_beats: last.fade_out_trail_beats,
        ..first
    })
}

/// run を 1 つの event とみなして `f` を掛け、元の片の窓 (位置・長さ・安定 id) で切り直して書き戻す。
/// `f` は位置と長さを変えない編集 (値 / fade / 写像)。
pub fn edit_run<E: TimedEvent>(events: &mut [E], run: &[usize], f: impl FnOnce(&mut E)) {
    let Some(mut joined) = joined_run(events, run) else {
        return;
    };
    f(&mut joined);
    if let [only] = run {
        events[*only] = joined;
        return;
    }
    for &i in run {
        let orig = &events[i];
        let (start, len) = (orig.start(), orig.len());
        let mut piece = event_piece(&joined, start, start + len);
        // 窓は元の値のまま (浮動小数の往復で片の端を 1 ulp も動かさない)。
        piece.set_window(start, len);
        piece.keep_identity(orig);
        events[i] = piece;
    }
}

/// `targets` をひと続きの片ごとに `f` で編集する (run ごとに 1 回)。 編集した run の数。
pub fn edit_runs<E: TimedEvent>(events: &mut [E], targets: &[usize], mut f: impl FnMut(&mut E)) -> usize {
    let runs = piece_runs(events, targets);
    for run in &runs {
        edit_run(events, run, &mut f);
    }
    runs.len()
}

impl ClipContent {
    /// r.md #38: クリップの窓 `window` に見えている event の fade を **content 種別に依らず**、ひと続きの
    /// 片ごとに 1 つ列挙する (`(ひと続きの先頭の event の index, ひと続きの fade)`、開始拍順)。
    ///
    /// `AudioEvent` / `VideoEvent` / `ImageEvent` / `TextEvent` は位置・長さ・両端の fade を同じ意味で
    /// 持ち、適用側も全部 [`crate::audio_render::fade_curve_at`] を通るので、「fade をどう描き、どう掴み、
    /// どう編集するか」は content に依存しない。 アレンジ画面の描画 / hit-test / drag はこの 1 本を SSoT に
    /// する。 分割の片は切り口に掴み所を出さず、ひと続きの外側の端だけに出す (分割は切れ目を入れるだけ)。
    /// `Midi` / `Automation` は fade を持たないので空。
    #[must_use]
    pub fn window_fades(&self, window: (f64, f64)) -> Vec<(usize, EventFade)> {
        fn collect<E: TimedEvent>(events: &[E], window: (f64, f64)) -> Vec<(usize, EventFade)> {
            piece_runs(events, &shown_indices(events, window))
                .iter()
                .filter_map(|run| Some((*run.first()?, run_fade(events, run)?)))
                .collect()
        }
        match self {
            ClipContent::Audio(c) => collect(&c.events, window),
            ClipContent::Video(c) => collect(&c.events, window),
            ClipContent::Image(c) => collect(&c.events, window),
            ClipContent::Text(c) => collect(&c.events, window),
            ClipContent::Midi(_) | ClipContent::Automation(_) => Vec::new(),
        }
    }

    /// r.md #38: 窓 `window` の中で `index` 番目の event を含むひと続きの fade を、content 種別に依らず
    /// 書き換える ([`Self::window_fades`] の書き込み口)。 `f` には現在のひと続きの fade を渡し、戻り値の
    /// 長さ / curve / ランプの張り出しをひと続きの両端へ書き戻す (clamp は caller の責務 =
    /// [`EventFade::len_beats`] を上限にする。 端から掛け直す fade は張り出しを 0 にして渡す)。
    ///
    /// event が窓に見えていない / fade を持たない content なら `false`。
    pub fn set_window_fade(
        &mut self,
        window: (f64, f64),
        index: usize,
        f: impl FnOnce(EventFade) -> EventFade,
    ) -> bool {
        fn apply<E: TimedEvent>(
            events: &mut [E],
            window: (f64, f64),
            index: usize,
            f: impl FnOnce(EventFade) -> EventFade,
        ) -> bool {
            let runs = piece_runs(events, &shown_indices(events, window));
            let Some(run) = runs.into_iter().find(|run| run.contains(&index)) else {
                return false;
            };
            edit_run(events, &run, |joined| {
                let next = f(joined.fade());
                joined.set_fade(&next);
            });
            true
        }
        match self {
            ClipContent::Audio(c) => apply(&mut c.events, window, index, f),
            ClipContent::Video(c) => apply(&mut c.events, window, index, f),
            ClipContent::Image(c) => apply(&mut c.events, window, index, f),
            ClipContent::Text(c) => apply(&mut c.events, window, index, f),
            ClipContent::Midi(_) | ClipContent::Automation(_) => false,
        }
    }
}

/// [`Song::crossfade_adjacent`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crossfade {
    /// 境界にクロスフェードを掛けた。
    pub applied: bool,
    /// Song を書き換えた (掛けなくても、窓の端を跨ぐ片を切ったら `true`)。
    pub changed: bool,
}

/// クロスフェードの片側: 窓の端に接する片のひと続き。
struct XfadeSide {
    content_id: super::ContentId,
    run: Vec<usize>,
    /// 端の外に鳴らせる素材の拍 (前 = 尻の先 / 次 = 頭の手前)。
    room: f64,
    /// ひと続きの長さ (拍)。
    len: f64,
}

impl Song {
    /// Auto-Crossfade の 1 ペア: 同じトラックで隣り合う audio クリップ `prev` (境界で終わる) と `next`
    /// (境界で始まる) の境界にクロスフェードを掛ける。 掛けたら `true`。
    ///
    /// **持ち方は窓の定義から決める**:
    /// - 鳴らし合う素材は、それぞれの窓の端に接する片 (のひと続き) の **take の続き** — 前のクリップは
    ///   窓の末尾で終わる片の隠れている尻を境界の先まで、次のクリップは窓の先頭から始まる片の隠れている
    ///   頭を境界の手前から鳴らす (張り出しは `Clip::xfade_*`、鳴らすのは `audio_clip_renderer`)。 窓の外の
    ///   別の片は鳴らさない — content を共有する 2 クリップ (分割の片) で隣の片を 2 回鳴らしていたのを直す。
    /// - 重なる区間は `[境界 - lead, 境界 + tail]` の 1 本で、前の fade-out と次の fade-in がちょうどこの
    ///   区間を覆う。 `lead` / `tail` は `xfade_beats / 2` を上限に、**鳴らせる素材がある分だけ** (take の
    ///   隠れている分 + source の残り、ランプを載せる相手側の片の長さ)。 区間を両側で揃えるので、素材が
    ///   足りない側で音量が落ち込まない。 分割の片同士は同じ素材を同じ位置で鳴らすので、分割前と同じ音になる。
    /// - 端の片が窓の端を跨いでいれば、先に窓の端で切る (切れ目を入れるだけ = 音は変わらない)。
    pub fn crossfade_adjacent(&mut self, prev: ClipKey, next: ClipKey, xfade_beats: f64) -> Crossfade {
        // 先に両側の端で切ってから片を探す (content を共有していると、後から切ると前に探した index がずれる)。
        let cut = self.cut_at_window_edge(prev, true) | self.cut_at_window_edge(next, false);
        let not_applied = Crossfade { applied: false, changed: cut };
        let (Some(p), Some(n)) = (self.xfade_side(prev, true), self.xfade_side(next, false)) else {
            return not_applied;
        };
        let half = (xfade_beats * 0.5).max(0.0);
        let tail = half.min(p.room).min(n.len);
        let lead = half.min(n.room).min(p.len);
        let ramp = lead + tail;
        if ramp <= EPS {
            return not_applied;
        }
        let bpm = self.bpm;
        let sources = &self.media.audio_sources;
        if let Some(ClipContent::Audio(a)) = self.clip_contents.get_mut(&p.content_id) {
            edit_run(&mut a.events, &p.run, |e| {
                let (fpb, frames) = source_reading(sources, bpm, e.source_id);
                e.reserve_take_tail(tail, fpb, frames);
                (e.fade_out_beats, e.fade_out_trail_beats) = (ramp, tail);
            });
        }
        if let Some(ClipContent::Audio(a)) = self.clip_contents.get_mut(&n.content_id) {
            edit_run(&mut a.events, &n.run, |e| {
                let (fpb, frames) = source_reading(sources, bpm, e.source_id);
                e.reserve_take_head(lead, fpb, frames);
                (e.fade_in_beats, e.fade_in_lead_beats) = (ramp, lead);
            });
        }
        if let Some(c) = self.clip_by_key_mut(prev) {
            c.xfade_tail_beats = tail;
        }
        if let Some(c) = self.clip_by_key_mut(next) {
            c.xfade_lead_beats = lead;
        }
        Crossfade { applied: true, changed: true }
    }

    /// クリップ `key` の窓の端 (`at_end` = 末尾、`false` = 先頭) を跨ぐ audio の片を端で切る。 切ったら `true`。
    fn cut_at_window_edge(&mut self, key: ClipKey, at_end: bool) -> bool {
        let Some(clip) = self.clip_by_key(key) else {
            return false;
        };
        let (content_id, (lo, hi)) = (clip.content_id, clip.content_window());
        let edge = if at_end { hi } else { lo };
        let Some(content @ ClipContent::Audio(_)) = self.clip_contents.get(&content_id) else {
            return false;
        };
        if !Self::content_crosses(content, edge) {
            return false;
        }
        let cut = self.split_content_at_points(content_id, &[edge]);
        if let Some(clip) = self.clip_by_key_mut(key) {
            clip.content_id = cut;
        }
        true
    }

    /// クロスフェードの片側 (`at_end` = クリップの窓の末尾側、`false` = 先頭側): 窓の端に接して鳴る片の
    /// ひと続き。 無ければ `None`。
    fn xfade_side(&self, key: ClipKey, at_end: bool) -> Option<XfadeSide> {
        let clip = self.clip_by_key(key)?;
        let (content_id, window) = (clip.content_id, clip.content_window());
        let edge = if at_end { window.1 } else { window.0 };
        let ClipContent::Audio(audio) = self.clip_contents.get(&content_id)? else {
            return None;
        };
        let events = &audio.events;
        let runs = piece_runs(events, &shown_indices(events, window));
        let run = runs.into_iter().find(|run| {
            let edge_of = |i: &usize| {
                let e = &events[*i];
                if at_end { e.start() + e.len() } else { e.start() }
            };
            let touching = if at_end { run.last() } else { run.first() };
            touching.is_some_and(|i| (edge_of(i) - edge).abs() <= EPS)
        })?;
        let joined = joined_run(events, &run)?;
        let (fpb, frames) = source_reading(&self.media.audio_sources, self.bpm, joined.source_id);
        let room = if at_end { joined.room_after(fpb, frames) } else { joined.room_before(fpb, frames) };
        Some(XfadeSide { content_id, run, room, len: joined.len() })
    }
}

/// source を native rate で読む 1 拍あたりの frame と、ファイルの frame 数 (take を伸ばせる上限)。
/// 未登録の source は `(0, 0)` (伸ばせない)。
fn source_reading(
    sources: &std::collections::HashMap<super::AudioSourceId, super::AudioSource>,
    bpm: f32,
    id: super::AudioSourceId,
) -> (f64, u64) {
    sources
        .get(&id)
        .map_or((0.0, 0), |s| (f64::from(s.sample_rate) * 60.0 / f64::from(bpm.max(1.0)), s.frames))
}
