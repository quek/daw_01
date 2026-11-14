//! **時間軸を持つ event (audio / video / image / text) の窓** — 片の切り出しと、片のつなぎ直しの SSoT。
//!
//! 4 種の event は「clip 内の位置 / 長さ / 両端の fade」を同じ意味で持つ ([`TimedEvent`])。 分割の片は
//! 元の event の **窓** で、中身の写像には触らない (r.md #132 残件: 分割は切れ目を入れるだけで、分割直後の
//! 再生・書き出し・描画は分割前と同じ):
//!
//! - fade のランプが切り口を跨いでいれば、両側の片がランプの続きを持つ (`fade_in_lead_beats` /
//!   `fade_out_trail_beats`)。 切り口で fade を 0 に落とすと、ランプの途中でゲインが跳ぶ。
//! - source を時間で読む audio / video は take の窓 (`take_head_beats` / `take_tail_beats`) を進める。
//!   source の範囲を拍の比で配り直すと、伸縮 / warp / slice / テンポ変化 / 移調のどれでも写像がずれる。
//! - 字幕は後ろの片を続きの片 ([`TextEvent::continuation`]) にする — 読み上げは最初の片だけが 1 回。
//!
//! [`join_pieces`] は [`event_piece`] の逆で、同じ event から切り出した隣り合う片を元の 1 つに戻す
//! (Glue)。 片のあとで一方だけを編集した (fade を掛け直した / 別の素材に変えた) 片はつながない。

use super::{AudioEvent, ClipContent, EventFade, ImageEvent, TextEvent, VideoEvent};

/// 拍の同一視許容量 ([`super::split_boundaries`] と同じ)。
const EPS: f64 = 1e-9;

/// clip の中で時間の区間と fade を持つ event。
pub trait TimedEvent: Clone + PartialEq {
    /// clip 内の開始拍 (content-local)。
    fn start(&self) -> f64;
    /// 長さ (拍)。
    fn len_beats(&self) -> f64;
    /// 開始拍と長さを置き換える (中身の写像は動かさない)。
    fn set_window(&mut self, start: f64, len: f64);
    /// 位置と fade (長さ / curve / ランプの張り出し)。
    fn fade(&self) -> EventFade;
    /// fade の長さ / curve / ランプの張り出しを書き戻す (位置は無視する)。
    fn set_fade(&mut self, fade: &EventFade);
    /// fade-in を **左端から** 掛け直す (ランプの張り出しを捨てる)。 ユーザーの fade 編集の口。
    fn set_edge_fade_in(&mut self, beats: f64) {
        let mut f = self.fade();
        f.fade_in_beats = beats;
        f.fade_in_lead_beats = 0.0;
        self.set_fade(&f);
    }
    /// fade-out を **右端から** 掛け直す (ランプの張り出しを捨てる)。
    fn set_edge_fade_out(&mut self, beats: f64) {
        let mut f = self.fade();
        f.fade_out_beats = beats;
        f.fade_out_trail_beats = 0.0;
        self.set_fade(&f);
    }
    /// 見えている窓を take の中で `head` 拍後ろから始め、`tail` 拍手前で終える (source を時間で読む
    /// event だけが take を持つ。 それ以外は何もしない)。
    fn narrow_take(&mut self, _head: f64, _tail: f64) {}
    /// 先頭の片の続きにする (読み上げを持つ字幕だけ)。
    fn mark_continuation(&mut self) {}
    /// 窓・fade・take・続きの印・安定 id を除いた **中身** (結合できる素材かの比較用の正規形)。
    #[must_use]
    fn material(&self) -> Self;
    /// `next` がこの event の take の続き (take の窓がつながっていて、続きの片として作られた) か。
    fn take_continues_into(&self, next: &Self) -> bool;
    /// 結合: `next` の take の尻を写す (位置と fade は [`join_pieces`] が持つ)。
    fn absorb_take_tail(&mut self, _next: &Self) {}
    /// 片を作り直したとき、元の片の **安定 id** (event の id / take の id) を引き継ぐ
    /// (`window_edit::edit_run`)。 id を持たない event は何もしない。
    fn keep_identity(&mut self, _orig: &Self) {}
}

/// 4 種で同じ意味・同じ名前のフィールドの accessor。
macro_rules! timed_event_common {
    () => {
        fn start(&self) -> f64 {
            self.event_start_in_clip_beats
        }
        fn len_beats(&self) -> f64 {
            self.event_length_beats
        }
        fn set_window(&mut self, start: f64, len: f64) {
            self.event_start_in_clip_beats = start;
            self.event_length_beats = len;
        }
        fn fade(&self) -> EventFade {
            EventFade {
                start_in_clip_beats: self.event_start_in_clip_beats,
                len_beats: self.event_length_beats,
                fade_in_beats: self.fade_in_beats,
                fade_out_beats: self.fade_out_beats,
                fade_in_curve: self.fade_in_curve,
                fade_out_curve: self.fade_out_curve,
                fade_in_lead_beats: self.fade_in_lead_beats,
                fade_out_trail_beats: self.fade_out_trail_beats,
            }
        }
        fn set_fade(&mut self, fade: &EventFade) {
            self.fade_in_beats = fade.fade_in_beats;
            self.fade_out_beats = fade.fade_out_beats;
            self.fade_in_curve = fade.fade_in_curve;
            self.fade_out_curve = fade.fade_out_curve;
            self.fade_in_lead_beats = fade.fade_in_lead_beats;
            self.fade_out_trail_beats = fade.fade_out_trail_beats;
        }
    };
}

/// 窓と fade を正規化した複製 (`material` の共通部分)。
fn without_window<E: TimedEvent>(ev: &E) -> E {
    let mut m = ev.clone();
    m.set_window(0.0, 0.0);
    let mut f = m.fade();
    f.fade_in_beats = 0.0;
    f.fade_out_beats = 0.0;
    f.fade_in_curve = super::FadeCurve::Linear;
    f.fade_out_curve = super::FadeCurve::Linear;
    f.fade_in_lead_beats = 0.0;
    f.fade_out_trail_beats = 0.0;
    m.set_fade(&f);
    m
}

impl TimedEvent for AudioEvent {
    timed_event_common!();

    fn narrow_take(&mut self, head: f64, tail: f64) {
        self.take_head_beats += head;
        self.take_tail_beats += tail;
    }

    fn material(&self) -> Self {
        let mut m = without_window(self);
        // 片ごとに違う event の id は捨て、同じ take か (take の id) は中身として比べる。
        m.take_id = self.take_key();
        m.id = 0;
        m.take_head_beats = 0.0;
        m.take_tail_beats = 0.0;
        m
    }

    fn take_continues_into(&self, next: &Self) -> bool {
        (next.take_head_beats - (self.take_head_beats + self.event_length_beats)).abs() <= EPS
            && (self.take_tail_beats - (next.event_length_beats + next.take_tail_beats)).abs() <= EPS
    }

    fn absorb_take_tail(&mut self, next: &Self) {
        self.take_tail_beats = next.take_tail_beats;
    }

    fn keep_identity(&mut self, orig: &Self) {
        self.id = orig.id;
        self.take_id = orig.take_id;
    }
}

impl TimedEvent for VideoEvent {
    timed_event_common!();

    fn narrow_take(&mut self, head: f64, _tail: f64) {
        // 映像は take の頭からの実時間で source を読み、末尾は source 窓の終端で止まる
        // (尻の長さは写像に効かない) ので、進めるのは頭だけ。
        self.take_head_beats += head;
    }

    fn material(&self) -> Self {
        let mut m = without_window(self);
        m.take_head_beats = 0.0;
        m
    }

    fn take_continues_into(&self, next: &Self) -> bool {
        (next.take_head_beats - (self.take_head_beats + self.event_length_beats)).abs() <= EPS
    }
}

/// 伸縮の倍率 `factor` (= 新しい長さ ÷ 元の長さ) を掛けた後の再生速度。 退化した倍率では変えない。
fn stretched_speed(speed: f64, factor: f64) -> f64 {
    if factor.is_finite() && factor > 0.0 {
        super::event_speed_or_unity(super::event_speed_or_unity(speed) / factor)
    } else {
        speed
    }
}

impl ClipContent {
    /// クリップの time-stretch (Shift + 端 drag) を content 全体に掛ける: **content の拍の軸を
    /// `factor` 倍する** (event / note の開始拍と長さ)。 audio / 映像 / 画像は source の範囲を固定した
    /// まま時間写像を伸縮する (各 event の `apply_time_stretch`)。 伸縮の概念を持たない字幕 /
    /// automation は触らず `false` を返す。
    ///
    /// 呼び側はクリップの窓の起点 (`Clip::content_offset_beats`) も同じ `factor` 倍にする。 軸ごと
    /// 伸縮するので、窓より前に隠れている中身 (左端を trim したクリップ) も比例して写り、固定端
    /// (右端 drag なら左端) の song 上の位置は動かない。
    pub fn apply_time_stretch(&mut self, factor: f64) -> bool {
        if !factor.is_finite() || factor <= 0.0 {
            return false;
        }
        match self {
            ClipContent::Audio(audio) => {
                for e in &mut audio.events {
                    e.apply_time_stretch(e.event_start_in_clip_beats * factor, e.event_length_beats * factor, factor);
                }
            }
            ClipContent::Midi(midi) => {
                for n in &mut midi.notes {
                    n.start_beat *= factor;
                    n.duration_beats *= factor;
                }
            }
            ClipContent::Video(video) => {
                for e in &mut video.events {
                    e.apply_time_stretch(e.event_start_in_clip_beats * factor, e.event_length_beats * factor, factor);
                }
            }
            ClipContent::Image(image) => {
                for e in &mut image.events {
                    e.apply_time_stretch(e.event_start_in_clip_beats * factor, e.event_length_beats * factor, factor);
                }
            }
            ClipContent::Text(_) | ClipContent::Automation(_) => return false,
        }
        true
    }
}

impl VideoEvent {
    /// 再生に使う速度 (非有限 / 0 以下の保存値は等速として読む)。
    #[must_use]
    pub fn playback_speed(&self) -> f64 {
        super::event_speed_or_unity(self.speed)
    }

    /// クリップの time-stretch (Shift + 端 drag) を 1 event に掛ける ([`AudioEvent::apply_time_stretch`]
    /// の映像版): 見えている窓を `start` / `len` (content-local 拍) にし、take の頭までの拍を同じ
    /// 倍率 `factor` で伸縮して、速度を `1 / factor` 倍にする。 **source の範囲は固定** なので、
    /// 伸ばす前に各拍で映っていた frame が、伸ばした後の対応する拍で映る。 確定
    /// (`stretch_clip_content`) とゴーストが同じこれを通る。
    pub fn apply_time_stretch(&mut self, start: f64, len: f64, factor: f64) {
        self.event_start_in_clip_beats = start;
        self.event_length_beats = len;
        if factor.is_finite() && factor > 0.0 {
            self.take_head_beats *= factor;
        }
        self.speed = stretched_speed(self.speed, factor);
    }
}

impl ImageEvent {
    /// 再生に使う速度 (非有限 / 0 以下の保存値は等速として読む)。
    #[must_use]
    pub fn playback_speed(&self) -> f64 {
        super::event_speed_or_unity(self.speed)
    }

    /// クリップの time-stretch (Shift + 端 drag) を 1 event に掛ける: 見えている窓を `start` / `len`
    /// にし、アニメーションの速度を `1 / factor` 倍にする (1 周の拍数が `factor` 倍になる)。
    /// 静止画は窓が変わるだけ。
    pub fn apply_time_stretch(&mut self, start: f64, len: f64, factor: f64) {
        self.event_start_in_clip_beats = start;
        self.event_length_beats = len;
        self.speed = stretched_speed(self.speed, factor);
    }
}

impl TimedEvent for ImageEvent {
    timed_event_common!();

    fn material(&self) -> Self {
        without_window(self)
    }

    fn take_continues_into(&self, _next: &Self) -> bool {
        // 画像は take の頭を持たないので、中身が同じなら続きとして扱う。
        // アニメーション画像 (GIF / APNG / WebP) は各イベントの頭から再生するので、
        // 分割した後ろの片は 1 フレーム目から始まる (`image_compose` の `anim_secs`)。
        true
    }
}

impl TimedEvent for TextEvent {
    timed_event_common!();

    fn mark_continuation(&mut self) {
        self.continuation = true;
    }

    fn material(&self) -> Self {
        let mut m = without_window(self);
        m.continuation = false;
        m
    }

    fn take_continues_into(&self, next: &Self) -> bool {
        // 別々に置いた同じ文 (どちらも読み上げる) はつながない。 続きの片だけが前の片に戻る。
        next.continuation
    }
}

/// `ev` の窓 `[a, b)` (content-local 拍、`ev` の区間の内側) だけを見せる片。
///
/// - 位置と長さだけが `[a, b)` になり、中身の写像 (source / take / 本文) はそのまま。 audio / video は
///   take の窓を進め ([`TimedEvent::narrow_take`])、`a` が元の頭より後ろなら字幕は続きの片になる。
/// - fade のランプが片に掛かっていれば、片の端の外から始まる / 外で終わるランプとして続きを持つ。
///   掛かっていなければその端の fade は無し。
/// - 端ちょうど (`EPS` 以内) は元の値のまま使う — 浮動小数の往復で窓やランプの端を 1 ulp もずらさない。
#[must_use]
pub fn event_piece<E: TimedEvent>(ev: &E, a: f64, b: f64) -> E {
    let (start, len) = (ev.start(), ev.len_beats());
    let end = start + len;
    let head = if (a - start).abs() <= EPS { 0.0 } else { a - start };
    let tail = if (end - b).abs() <= EPS { 0.0 } else { end - b };
    let piece_start = if head == 0.0 { start } else { a };
    let piece_len = if head == 0.0 && tail == 0.0 {
        len
    } else {
        (if tail == 0.0 { end } else { b }) - piece_start
    };
    let f = ev.fade();
    let mut next = f;
    // fade-in のランプは event-local `[-lead, fade_in - lead)`、片は `[head, len - tail)`。
    if f.fade_in_beats > 0.0 && f.fade_in_beats - f.fade_in_lead_beats > head + EPS {
        next.fade_in_lead_beats = f.fade_in_lead_beats + head;
    } else {
        next.fade_in_beats = 0.0;
        next.fade_in_lead_beats = 0.0;
    }
    // fade-out のランプは event-local `[len + trail - fade_out, len + trail)`。
    if f.fade_out_beats > 0.0 && len + f.fade_out_trail_beats - f.fade_out_beats < len - tail - EPS {
        next.fade_out_trail_beats = f.fade_out_trail_beats + tail;
    } else {
        next.fade_out_beats = 0.0;
        next.fade_out_trail_beats = 0.0;
    }
    let mut piece = ev.clone();
    piece.set_window(piece_start, piece_len);
    piece.set_fade(&next);
    piece.narrow_take(head, tail);
    if head > 0.0 {
        piece.mark_continuation();
    }
    piece
}

/// `ev` を content-local の切り口 `cuts` で割った片 (先頭から、先頭の片 = 元の頭)。 使う切り口の規則
/// (区間の外 / 端ちょうどは無視、`min_piece` より短い片は隣へくっつける) は [`super::split_boundaries`]。
/// 割れなければ空。
#[must_use]
pub fn split_pieces<E: TimedEvent>(ev: &E, cuts: impl IntoIterator<Item = f64>, min_piece: f64) -> Vec<E> {
    let (start, len) = (ev.start(), ev.len_beats());
    let end = start + len;
    let kept = super::split_boundaries(start, end, cuts, min_piece);
    if kept.is_empty() {
        return Vec::new();
    }
    let bounds: Vec<f64> = std::iter::once(start).chain(kept).chain(std::iter::once(end)).collect();
    bounds.windows(2).map(|w| event_piece(ev, w[0], w[1])).collect()
}

/// `next` を `prev` の直後の片として 1 つにつなげるか ([`event_piece`] の逆) = 同じ event から切り出した
/// ままの隣り合う片 (「窓の中でひと続きの片」、`window_edit` の run の単位)。
pub(super) fn joinable<E: TimedEvent>(prev: &E, next: &E) -> bool {
    if (prev.start() + prev.len_beats() - next.start()).abs() > EPS
        || !prev.take_continues_into(next)
        || prev.material() != next.material()
    {
        return false;
    }
    let (p, n) = (prev.fade(), next.fade());
    // fade-in: 後ろの片がランプの続きを持つなら同じランプ、持たないなら前の片の中でランプが終わっている。
    let fade_in_ok = if n.fade_in_beats > 0.0 {
        n.fade_in_beats == p.fade_in_beats
            && n.fade_in_curve == p.fade_in_curve
            && (n.fade_in_lead_beats - (p.fade_in_lead_beats + p.len_beats)).abs() <= EPS
    } else {
        p.fade_in_beats <= 0.0 || p.fade_in_beats - p.fade_in_lead_beats <= p.len_beats + EPS
    };
    // fade-out: 前の片がランプの頭を持つなら同じランプ、持たないなら後ろの片の中でランプが始まる。
    let fade_out_ok = if p.fade_out_beats > 0.0 {
        p.fade_out_beats == n.fade_out_beats
            && p.fade_out_curve == n.fade_out_curve
            && (p.fade_out_trail_beats - (n.len_beats + n.fade_out_trail_beats)).abs() <= EPS
    } else {
        n.fade_out_beats <= 0.0 || n.len_beats + n.fade_out_trail_beats - n.fade_out_beats >= -EPS
    };
    fade_in_ok && fade_out_ok
}

/// 隣り合う片のうち、同じ event から [`event_piece`] で切り出したままのものを元の 1 つへつなぐ
/// (Glue の結合で使う)。 結果は開始拍順。 位置・中身・fade のどれかが分割の後で変わった片はつながない
/// (つなぐと再生が変わる)。
pub fn join_pieces<E: TimedEvent>(events: &mut Vec<E>) {
    events.sort_by(|a, b| a.start().total_cmp(&b.start()));
    let mut out: Vec<E> = Vec::with_capacity(events.len());
    for ev in events.drain(..) {
        let Some(prev) = out.iter_mut().rev().find(|p| joinable(*p, &ev)) else {
            out.push(ev);
            continue;
        };
        join_into(prev, &ev);
    }
    *events = out;
}

/// `prev` の直後の片 `next` を `prev` へつなぐ (呼び側が [`joinable`] を確かめてある前提)。 窓は
/// `next` の末尾まで伸び、fade-out と take の尻は `next` のものになる。
pub(super) fn join_into<E: TimedEvent>(prev: &mut E, next: &E) {
    let start = prev.start();
    let n = next.fade();
    let mut joined = prev.fade();
    joined.fade_out_beats = n.fade_out_beats;
    joined.fade_out_curve = n.fade_out_curve;
    joined.fade_out_trail_beats = n.fade_out_trail_beats;
    prev.set_window(start, next.start() + next.len_beats() - start);
    prev.set_fade(&joined);
    prev.absorb_take_tail(next);
}
