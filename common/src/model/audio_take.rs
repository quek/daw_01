//! audio event の **take** (時間写像の単位) と、それを見せる窓の操作。
//!
//! [`AudioEvent::take_head_beats`] の doc が模型の正本。 ここは take の量の読み出しと、窓を動かす編集
//! (オーディオエディタの端 trim / 伸縮) を 1 か所に集める — どれも「写像はそのまま、窓だけを動かす」
//! (窓が take の外へ出るときだけ、take 自体を同じ伸縮率のまま source 側へ伸ばす)。

use std::collections::HashMap;

use super::{AudioContent, AudioEvent, StretchMode};

impl AudioEvent {
    /// この event が属する take の id ([`Self::take_id`] の doc)。 分割していない event は自分の `id`。
    #[must_use]
    pub fn take_key(&self) -> u32 {
        if self.take_id == 0 { self.id } else { self.take_id }
    }

    /// take の長さ (拍) = 見えている長さ + 頭と尻の隠れている分。 `source_*_frames` の窓はこの長さへ
    /// 写る (伸縮率 `common::audio_render::stretch_ratio_for` の分母)。
    #[must_use]
    pub fn take_length_beats(&self) -> f64 {
        self.take_head_beats + self.event_length_beats + self.take_tail_beats
    }

    /// take の頭が置かれる content-local 拍 (= 時間写像の 0 点)。
    #[must_use]
    pub fn take_start_in_clip_beats(&self) -> f64 {
        self.event_start_in_clip_beats - self.take_head_beats
    }

    /// source 窓の長さ (frame)。
    #[must_use]
    pub fn source_window_frames(&self) -> u64 {
        self.source_end_frames.saturating_sub(self.source_start_frames)
    }

    /// take の 1 拍あたりの source frame (= 伸縮率込みの配置の速さ)。 take が退化していれば
    /// `fallback` (呼び側の native rate)。
    #[must_use]
    pub fn take_frames_per_beat(&self, fallback: f64) -> f64 {
        let take_len = self.take_length_beats();
        let window = self.source_window_frames();
        #[allow(clippy::cast_precision_loss)]
        let rate = window as f64 / take_len;
        if take_len > 1e-9 && window > 0 && rate.is_finite() { rate } else { fallback }
    }

    /// クリップの time-stretch (Shift+端 drag) を 1 event に掛ける: 見えている窓を `start` / `len`
    /// (content-local 拍) にし、take の軸を同じ倍率 `factor` で伸縮して ([`Self::scale_take`])、Raw
    /// (= 時間操作しない定義) はピッチ保持の Stretch へ昇格する (既に Repitch / Stretch / Slice なら維持)。
    /// **source 窓は固定** = これが stretch の本質。 確定 (`stretch_clip_content`) とゴースト
    /// (`content_build`) が同じこれを通る (写経すると伸縮した中身とゴーストが食い違う)。
    pub fn apply_time_stretch(&mut self, start: f64, len: f64, factor: f64) {
        self.event_start_in_clip_beats = start;
        self.event_length_beats = len;
        self.scale_take(factor);
        if self.stretch_mode == StretchMode::Raw {
            self.stretch_mode = StretchMode::Stretch;
        }
    }

    /// 伸縮 (クリップの Shift+端 drag / Raw の BPM 追従) で **take の拍の軸** を `factor` 倍する:
    /// take の隠れている頭と尻、warp marker の拍。 見えている位置と長さは呼び側が同じ倍率で写す。
    pub fn scale_take(&mut self, factor: f64) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.take_head_beats *= factor;
        self.take_tail_beats *= factor;
        for m in &mut self.beat_markers {
            m.locked_beat *= factor;
        }
    }

    /// take を **見えている窓だけに詰め直す** (take の頭 / 尻を 0 にし、時間写像の起点を窓の頭へ移す)。
    /// `native_fpb` は source を native rate で読む 1 拍あたりの frame (Raw / slice 本体の読み速度、
    /// `source_sr * 60 / bpm`)。
    ///
    /// 分割の片は take (分割する前の event) の写像をそのまま使うので、写像の起点は片の外 (take の頭) に
    /// ある。 そのまま **移調 / 逆再生 / 伸縮 mode** を変えると、効き始めが take の頭になり、片の見えている
    /// 頭の音が別の場所へ跳ぶ (逆再生なら前の片の音を逆に読む)。 これらを変える直前にこれを呼ぶと、
    /// 片自身の頭を起点に効く (分割していない event に掛けたのと同じ)。 見えている窓の source 区間は
    /// 今の写像で求める (その mode が窓の頭と尻で読んでいる位置) ので、詰め直した直後の音は変わらない
    /// (テンポ曲線や stretch した slice の途中などの近似を除く — 直後に写像そのものを変える前提の操作)。
    pub fn rebase_take(&mut self, native_fpb: f64) {
        if self.take_head_beats == 0.0 && self.take_tail_beats == 0.0 {
            return;
        }
        let head = self.take_head_beats;
        let window = self.source_window_frames();
        #[allow(clippy::cast_precision_loss)]
        let win = window as f64;
        let p0 = self.read_pos_at(head, native_fpb).clamp(0.0, win);
        let p1 = self.read_pos_at(head + self.event_length_beats, native_fpb).clamp(p0, win);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (shift, end) = (p0.round() as u64, (p1.round() as u64).min(window));
        let new_window = end.saturating_sub(shift).max(1).min(window.saturating_sub(shift).max(1));
        let (old_start, old_window) = (self.source_start_frames, window);
        if self.reversed {
            // 逆再生の読み位置 p は frame `窓の頭 + 窓 − 1 − p`。 窓の中の [p0, p1) は source の尻側。
            self.source_start_frames = old_start + old_window.saturating_sub(shift + new_window);
        } else {
            self.source_start_frames = old_start + shift;
        }
        self.source_end_frames = self.source_start_frames + new_window;
        self.rebase_onsets(shift, new_window);
        let (ds, dw) = (self.source_start_frames as i128 - old_start as i128, new_window as i128 - old_window as i128);
        for m in &mut self.beat_markers {
            m.locked_beat -= head;
            if self.reversed {
                // 逆再生の読み位置 = 2・窓の頭 + 窓 − 1 − marker の frame を保つ。
                let sf = i128::from(m.source_frame) + 2 * ds + dw;
                m.source_frame = u64::try_from(sf.max(0)).unwrap_or(u64::MAX);
            }
        }
        self.take_head_beats = 0.0;
        self.take_tail_beats = 0.0;
    }

    /// take-local 拍 `beat` でこの event が読んでいる source 位置 (読み位置 = source 窓の頭から、
    /// 逆再生は読む向きの座標)。 一定テンポ (nominal) での写像 — `rebase_take` の窓の端を決めるため。
    fn read_pos_at(&self, beat: f64, native_fpb: f64) -> f64 {
        let rate = self.take_frames_per_beat(native_fpb);
        let pitch = crate::audio_render::pitch_factor(self.pitch_semitones);
        match self.stretch_mode {
            StretchMode::Raw => beat * native_fpb * pitch,
            StretchMode::Repitch => beat * rate * pitch,
            StretchMode::Stretch => {
                let mut markers = self.beat_markers.clone();
                markers.sort_by(|a, b| a.locked_beat.total_cmp(&b.locked_beat));
                #[allow(clippy::cast_precision_loss)]
                crate::audio_render::warp_source_frame(beat, &markers)
                    .map_or(beat * rate, |sf| sf - self.source_start_frames as f64)
            }
            StretchMode::Slice => {
                // 最後に鳴り始めた slice の頭から native rate で読み進めた位置 (次の slice の頭で止まる)。
                #[allow(clippy::cast_precision_loss)]
                let trigger = |o: u64| o as f64 / rate;
                let mut onsets = self.onsets.clone();
                onsets.sort_unstable();
                match onsets.iter().rposition(|&o| trigger(o) <= beat) {
                    #[allow(clippy::cast_precision_loss)]
                    Some(k) => {
                        let read = onsets[k] as f64 + (beat - trigger(onsets[k])) * native_fpb * pitch;
                        onsets.get(k + 1).map_or(read, |&next| read.min(next as f64))
                    }
                    None if onsets.is_empty() => beat * native_fpb * pitch,
                    None => beat * rate,
                }
            }
        }
    }

    /// `rebase_take` の onset: 窓の頭 (`shift`) より前を捨てて数え直し、窓の頭が slice の途中なら
    /// 窓の頭にも slice の始まりを置く (続きを鳴らす)。
    fn rebase_onsets(&mut self, shift: u64, new_window: u64) {
        if self.onsets.is_empty() {
            return;
        }
        let inside_slice = self.onsets.iter().any(|&o| o <= shift);
        let mut kept: Vec<u64> =
            self.onsets.iter().filter(|&&o| o >= shift && o - shift < new_window).map(|&o| o - shift).collect();
        kept.sort_unstable();
        if inside_slice && kept.first() != Some(&0) {
            kept.insert(0, 0);
        }
        self.onsets = kept;
    }

    /// 見えている末尾の **先で鳴らせる素材** の拍: take の隠れている尻と、take を同じ伸縮率のまま source の
    /// 残り (ファイルの末尾 `file_frames` まで) へ伸ばせる分。 クロスフェードの張り出しの上限。
    #[must_use]
    pub fn room_after(&self, fallback_fpb: f64, file_frames: u64) -> f64 {
        let spare = if self.reversed {
            self.source_start_frames
        } else {
            file_frames.saturating_sub(self.source_end_frames)
        };
        self.take_tail_beats.max(0.0) + frames_as_beats(spare, self.take_frames_per_beat(fallback_fpb))
    }

    /// 見えている先頭の **手前で鳴らせる素材** の拍 ([`Self::room_after`] の対)。
    #[must_use]
    pub fn room_before(&self, fallback_fpb: f64, file_frames: u64) -> f64 {
        let spare = if self.reversed {
            file_frames.saturating_sub(self.source_end_frames)
        } else {
            self.source_start_frames
        };
        self.take_head_beats.max(0.0) + frames_as_beats(spare, self.take_frames_per_beat(fallback_fpb))
    }

    /// take の隠れている尻を少なくとも `beats` 拍にする (足りない分だけ take を source の残りへ伸ばす。
    /// 見えている音は 1 frame も動かない)。
    pub fn reserve_take_tail(&mut self, beats: f64, fallback_fpb: f64, file_frames: u64) {
        let short = beats - self.take_tail_beats;
        if short > 0.0 {
            self.extend_take_tail(short, fallback_fpb, file_frames);
        }
    }

    /// take の隠れている頭を少なくとも `beats` 拍にする ([`Self::reserve_take_tail`] の対)。
    pub fn reserve_take_head(&mut self, beats: f64, fallback_fpb: f64, file_frames: u64) {
        let short = beats - self.take_head_beats;
        if short > 0.0 {
            self.extend_take_head(short, fallback_fpb, file_frames);
        }
    }

    /// 左端 trim: `delta` > 0 で内側へ縮め、< 0 で外へ伸ばす。 **窓を動かすだけ** で、見えている音は
    /// 1 frame も動かない。 take の頭より外へ伸ばすときは take を source の手前へ伸ばす (ファイルの
    /// 先頭 `file_frames` まで)。 長さは `min_len` を、開始拍は clip の頭 (0) を下回らない。
    pub fn trim_left(&mut self, delta: f64, fallback_fpb: f64, file_frames: u64, min_len: f64) {
        let mut d = delta
            .min((self.event_length_beats - min_len).max(0.0))
            .max(-self.event_start_in_clip_beats);
        if !d.is_finite() {
            return;
        }
        if d < 0.0 && self.take_head_beats + d < 0.0 {
            self.extend_take_head(-(self.take_head_beats + d), fallback_fpb, file_frames);
            d = d.max(-self.take_head_beats.max(0.0));
        }
        self.event_start_in_clip_beats += d;
        self.event_length_beats -= d;
        self.take_head_beats += d;
    }

    /// 右端 trim: `delta` > 0 で外へ伸ばし、< 0 で内側へ縮める ([`Self::trim_left`] の対)。
    pub fn trim_right(&mut self, delta: f64, fallback_fpb: f64, file_frames: u64, min_len: f64) {
        let mut d = delta.max(-(self.event_length_beats - min_len).max(0.0));
        if !d.is_finite() {
            return;
        }
        if d > 0.0 && self.take_tail_beats - d < 0.0 {
            self.extend_take_tail(d - self.take_tail_beats, fallback_fpb, file_frames);
            d = d.min(self.take_tail_beats.max(0.0));
        }
        self.event_length_beats += d;
        self.take_tail_beats -= d;
    }

    /// take を頭の側へ `beats` 拍伸ばす (見えている音は動かさない)。 伸ばせた拍を返す。
    ///
    /// 伸縮率 (take の 1 拍あたりの frame) を保ったまま source 窓を広げ、source 窓の頭を起点に持つ
    /// 量 (slice の onset) と、take の頭を起点に持つ量 (warp marker の拍) を同じだけずらす。 逆再生は
    /// take の頭が source 窓の **末尾** 側にあるので、末尾を伸ばす。
    fn extend_take_head(&mut self, beats: f64, fallback_fpb: f64, file_frames: u64) -> f64 {
        let rate = self.take_frames_per_beat(fallback_fpb);
        let available = if self.reversed {
            file_frames.saturating_sub(self.source_end_frames)
        } else {
            self.source_start_frames
        };
        let Some((frames, granted)) = take_extension(beats, rate, available) else {
            return 0.0;
        };
        if self.reversed {
            self.source_end_frames += frames;
            // 逆再生の読み位置 = 窓の頭 + (窓 − 1 − u)、u = marker の frame − 窓の頭。 窓が伸びた分だけ
            // u も進めないと同じ frame を読まない。
            for m in &mut self.beat_markers {
                m.source_frame = m.source_frame.saturating_add(frames);
            }
        } else {
            self.source_start_frames -= frames;
        }
        // onset は読み位置の座標 (窓の頭から) なので、頭に足した frame だけ後ろへずれる。 take の頭が
        // slice の始まりだったなら、伸ばした区間の頭も slice の始まりにする (置かないと伸ばした区間は
        // 最初の trigger まで無音)。 頭が始まりでなかった (= 頭は無音だった) なら置かない — 置くと
        // 元から見えていた無音の区間まで鳴り出す。
        if !self.onsets.is_empty() {
            let head_was_trigger = self.onsets.first() == Some(&0);
            for o in &mut self.onsets {
                *o = o.saturating_add(frames);
            }
            if head_was_trigger {
                self.onsets.insert(0, 0);
            }
        }
        for m in &mut self.beat_markers {
            m.locked_beat += granted;
        }
        self.take_head_beats += granted;
        granted
    }

    /// take を尻の側へ `beats` 拍伸ばす ([`Self::extend_take_head`] の対)。
    fn extend_take_tail(&mut self, beats: f64, fallback_fpb: f64, file_frames: u64) -> f64 {
        let rate = self.take_frames_per_beat(fallback_fpb);
        let available = if self.reversed {
            self.source_start_frames
        } else {
            file_frames.saturating_sub(self.source_end_frames)
        };
        let Some((frames, granted)) = take_extension(beats, rate, available) else {
            return 0.0;
        };
        if self.reversed {
            // 逆再生の尻は source 窓の頭側。 窓の頭が下がると同じ読み位置の u が変わらないよう、
            // marker の frame を同じだけ下げる (読み位置 = 2・窓の頭 + 窓 − 1 − marker の frame)。
            self.source_start_frames -= frames;
            for m in &mut self.beat_markers {
                m.source_frame = m.source_frame.saturating_sub(frames);
            }
        } else {
            self.source_end_frames += frames;
        }
        self.take_tail_beats += granted;
        granted
    }
}

impl AudioContent {
    /// 別の場所から来た event 群 (貼り付け / 複製) をこの content の **新しい id と新しい take** で足し、
    /// 足した位置 (index) を返す。
    ///
    /// 来た event の中で同じ take だった片同士は、ここでも同じ take にまとめる (片を並べて貼ったら
    /// Glue で元に戻せる)。 元の take とは別の take になる — ARA の編集 (audio modification) は
    /// content と take で 1 つなので、写した先は元と編集を共有しない。
    pub fn adopt_events(&mut self, events: impl IntoIterator<Item = AudioEvent>) -> Vec<usize> {
        let mut takes: HashMap<u32, u32> = HashMap::new();
        let mut added = Vec::new();
        for mut ev in events {
            let old_take = ev.take_key();
            ev.id = self.alloc_event_id();
            let take = *takes.entry(old_take).or_insert(ev.id);
            ev.take_id = if take == ev.id { 0 } else { take };
            added.push(self.events.len());
            self.events.push(ev);
        }
        added
    }
}

/// `frames` を伸縮率 `rate` (frame / 拍) で拍にする (退化した率は 0 拍)。
fn frames_as_beats(frames: u64, rate: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let beats = frames as f64 / rate;
    if rate.is_finite() && rate > 0.0 { beats } else { 0.0 }
}

/// `beats` 拍を伸縮率 `rate` (frame / 拍) で frame にし、使える `available` frame に収める。
/// `(frame, 実際に伸ばせた拍)`。 伸ばせなければ `None`。
fn take_extension(beats: f64, rate: f64, available: u64) -> Option<(u64, f64)> {
    if !(beats > 0.0 && rate.is_finite() && rate > 0.0) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let frames = ((beats * rate).round().max(0.0) as u64).min(available);
    #[allow(clippy::cast_precision_loss)]
    (frames > 0).then(|| (frames, frames as f64 / rate))
}
