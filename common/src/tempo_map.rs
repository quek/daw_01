//! Precomputed beat↔seconds map for the `SongTempo` automation curve, so
//! per-frame / RT callers convert between song beats and wall time in O(log n)
//! while honoring tempo automation — instead of an O(n) re-integration every
//! call or a wrong constant-bpm linear estimate (r.md #8 A4 video / A10 live
//! seek+loop-wrap; the offline export A2 integrates directly).
//!
//! Build off the audio thread (on song change) — `from_song` is O(song length).
//! The lookups (`beat_to_seconds` / `seconds_to_beat` and the sample variants)
//! are alloc/lock-free and RT-safe.

use crate::automation::SongTempoCurve;
use crate::model::Song;

/// Integration / table resolution. 1/16 beat keeps a 600-beat song to ~9600
/// breakpoints (~75 KB) while resolving typical tempo ramps smoothly.
const STEP_BEATS: f64 = 1.0 / 16.0;

/// この曲がテンポカーブを持つか (= [`song_beat_to_seconds`] が [`TempoMap`] の
/// 構築に落ちるか)。 呼び出し側が `TempoMap` を世代キャッシュする判断に使う。
#[must_use]
pub fn has_tempo_automation(song: &Song) -> bool {
    crate::automation::has_song_tempo_automation(song)
}

/// Song beat → song 秒の便宜関数。 `SongTempo` automation lane があれば
/// [`TempoMap`] を積分して写像し、 無ければ constant-bpm 換算の高速経路
/// (table 構築なし)。 per-frame 呼び出し想定 — automation を持つ曲のみ
/// O(song length) の build が走る (video_playback の per-frame 経路と同じ
/// 受容コスト)。 毎 frame 多数回呼ぶ場合は `TempoMap` を手で持ち回ること。
#[must_use]
pub fn song_beat_to_seconds(song: &Song, beat: f64) -> f64 {
    if has_tempo_automation(song) {
        TempoMap::from_song(song).beat_to_seconds(beat)
    } else {
        beat * 60.0 / f64::from(song.bpm.max(1.0))
    }
}

/// Beat→seconds lookup table for one song's tempo curve.
pub struct TempoMap {
    /// Cumulative elapsed seconds at breakpoint `i` (= beat `i * STEP_BEATS`).
    /// `secs[0] == 0.0`, strictly increasing (bpm is clamped ≥ 1).
    secs: Vec<f64>,
    /// bpm at/after the table end, used to extrapolate beyond it.
    tail_bpm: f64,
    /// 曲がテンポカーブを持つか ([`Self::is_automated`])。
    automated: bool,
    /// テンポが動く曲か ([`Self::varies`])。
    varies: bool,
}

impl TempoMap {
    /// Integrate the song's `SongTempo` curve into a cumulative seconds table.
    /// Constant-bpm songs reduce to a uniform `60/bpm` per beat (= linear).
    #[must_use]
    pub fn from_song(song: &Song) -> Self {
        // 表は曲末とテンポカーブの終端の遠い方まで張る (カーブが曲末より先へ伸びていると、範囲の書き出しや
        // ループがそこを読む)。上限は破損した project で `steps` が溢れて確保が OOM / panic するのを防ぐ (r.md #8 L9)。
        let curve_end = crate::automation::song_tempo_lane(song)
            .into_iter()
            .flat_map(|lane| lane.clips.iter())
            .map(|c| c.start_beat + c.length_beats)
            .filter(|end| end.is_finite())
            .fold(0.0, f64::max);
        let end_beat = crate::timing::song_end_beat(song)
            .max(curve_end)
            .clamp(1.0, crate::timing::MAX_SONG_BEATS);
        let steps = (end_beat / STEP_BEATS).ceil() as usize;
        let mut secs = Vec::with_capacity(steps + 1);
        secs.push(0.0);
        let mut s = 0.0_f64;
        let mut beat = 0.0_f64;
        // 刻みごとに song_lanes と clip 列を探さない (1 度引いて使い回す)。
        let curve = SongTempoCurve::of(song);
        for _ in 0..steps {
            // 中点則 (segment 中央の bpm) で積分精度を上げる (左 Riemann は ramp で
            // 系統誤差が出る)。 テンポは [1, 1000] clamp 済 → 0 除算なし。
            let bpm = f64::from(curve.at(beat + STEP_BEATS * 0.5));
            s += STEP_BEATS * 60.0 / bpm;
            secs.push(s);
            beat += STEP_BEATS;
        }
        let tail_bpm = f64::from(curve.at(end_beat));
        let automated = curve.is_automated();
        let modulated = song
            .song_mod_routings
            .iter()
            .any(|r| r.enabled && r.target == crate::model::AutomationTarget::SongTempo);
        Self { secs, tail_bpm, automated, varies: automated || modulated }
    }

    /// 元の曲がテンポカーブを持つか。RT が `song_lanes` を探さずに判定するための値。
    #[must_use]
    pub fn is_automated(&self) -> bool {
        self.automated
    }

    /// 元の曲のテンポが動くか (テンポカーブかテンポ宛の変調がある)。動く曲では、表の拍と鳴っている拍が
    /// 一致するとは限らない (表は変調を含まず、1/16 拍より細かい段差も丸める)。
    #[must_use]
    pub fn varies(&self) -> bool {
        self.varies
    }

    fn last_beat(&self) -> f64 {
        (self.secs.len() - 1) as f64 * STEP_BEATS
    }

    /// Song beat → elapsed seconds from beat 0 (tempo-integrated).
    #[must_use]
    pub fn beat_to_seconds(&self, beat: f64) -> f64 {
        if beat <= 0.0 {
            return 0.0;
        }
        let idx_f = beat / STEP_BEATS;
        // 巨大な拍では `as usize` が usize::MAX に飽和する。`i + 1` と書くと溢れて panic する
        // (`secs` は必ず 1 要素以上ある)。
        let i = idx_f.floor() as usize;
        if i < self.secs.len() - 1 {
            let frac = idx_f - i as f64;
            self.secs[i] + (self.secs[i + 1] - self.secs[i]) * frac
        } else {
            // 表外: 末尾から tail_bpm で外挿。
            let last = *self.secs.last().unwrap_or(&0.0);
            last + (beat - self.last_beat()) * 60.0 / self.tail_bpm
        }
    }

    /// Elapsed seconds → song beat (inverse of [`beat_to_seconds`]).
    #[must_use]
    pub fn seconds_to_beat(&self, seconds: f64) -> f64 {
        if seconds <= 0.0 {
            return 0.0;
        }
        let last = *self.secs.last().unwrap_or(&0.0);
        if seconds >= last {
            // 表外: tail_bpm で外挿。
            return self.last_beat() + (seconds - last) * self.tail_bpm / 60.0;
        }
        // secs は単調増加。 seconds を含むセル [i-1, i] を二分探索。
        let i = self.secs.partition_point(|&s| s <= seconds).max(1);
        let lo = self.secs[i - 1];
        let hi = self.secs[i];
        let frac = if hi > lo { (seconds - lo) / (hi - lo) } else { 0.0 };
        ((i - 1) as f64 + frac) * STEP_BEATS
    }

    /// Song beat → output sample index at `sample_rate`.
    #[must_use]
    pub fn beat_to_samples(&self, beat: f64, sample_rate: u32) -> u64 {
        (self.beat_to_seconds(beat) * f64::from(sample_rate)).round().max(0.0) as u64
    }

    /// Output sample index → song beat at `sample_rate`.
    #[must_use]
    pub fn samples_to_beat(&self, samples: u64, sample_rate: u32) -> f64 {
        if sample_rate == 0 {
            return 0.0;
        }
        self.seconds_to_beat(samples as f64 / f64::from(sample_rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, ClipContent, Song,
    };

    fn const_song(bpm: f32, len: f64) -> Song {
        Song {
            bpm,
            length_beats: len,
            ..Song::default()
        }
    }

    fn ramp_song(start: f32, end: f32, len: f64) -> Song {
        let mut song = const_song(start, len);
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                next_point_id: 0,
                points: vec![
                    AutomationPoint { id: 0, time_beat: 0.0, value: f64::from(start), curve: AutomationCurve::Linear },
                    AutomationPoint { id: 0, time_beat: len, value: f64::from(end), curve: AutomationCurve::Linear },
                ],
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip { id: 1, name: "t".into(), start_beat: 0.0, length_beats: len, content_id: cid, content_offset_beats: 0.0, color: None }],
            ..AutomationLane::new(AutomationTarget::SongTempo, f64::from(start))
        });
        song
    }

    #[test]
    fn constant_bpm_is_linear_and_inverts() {
        let m = TempoMap::from_song(&const_song(120.0, 16.0));
        // 120 bpm → 0.5 s/beat。
        assert!((m.beat_to_seconds(4.0) - 2.0).abs() < 1e-6, "{}", m.beat_to_seconds(4.0));
        assert!((m.beat_to_seconds(8.0) - 4.0).abs() < 1e-6);
        // round trip。
        assert!((m.seconds_to_beat(2.0) - 4.0).abs() < 1e-3);
        // sample 変換 (48k)。
        assert_eq!(m.beat_to_samples(4.0, 48_000), 96_000);
        assert!((m.samples_to_beat(96_000, 48_000) - 4.0).abs() < 1e-3);
        // 表外外挿 (一定なので線形継続)。
        assert!((m.beat_to_seconds(100.0) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn tempo_ramp_integrates_and_round_trips() {
        // 60→180 linear over 4 beats: ∫60/bpm db = 2 ln 3 ≈ 2.1972 s。
        let m = TempoMap::from_song(&ramp_song(60.0, 180.0, 4.0));
        let analytic = 2.0 * 3.0_f64.ln();
        assert!(
            (m.beat_to_seconds(4.0) - analytic).abs() < 0.01,
            "got {} want {analytic}",
            m.beat_to_seconds(4.0)
        );
        // constant-120 推定 (2.0 s) を上回る (平均テンポが時間加重で遅い)。
        assert!(m.beat_to_seconds(4.0) > 2.0);
        // round trip。
        let b = m.seconds_to_beat(analytic);
        assert!((b - 4.0).abs() < 0.05, "round trip beat {b}");
        // 中間 (beat 2) も単調。
        assert!(m.beat_to_seconds(2.0) > 0.0 && m.beat_to_seconds(2.0) < m.beat_to_seconds(4.0));
    }

    /// 表はクリップの終端と、曲末より先へ伸びたテンポカーブの終端まで積分する (音声の書き出しと同じ尺になる)。
    /// 許容差は 10 ms: 2 つの積分は刻みと公式が違い、このランプの 200 拍で約 4 ms 離れる
    /// (表がカーブの途中で切れて外挿に落ちると 14 秒離れる)。
    #[test]
    fn table_covers_clips_and_tempo_curve_beyond_length_beats() {
        let mut song = ramp_song(120.0, 60.0, 200.0);
        song.length_beats = 64.0;
        song.tracks.push(crate::model::Track {
            clips: vec![crate::model::Clip { start_beat: 96.0, length_beats: 4.0, ..Default::default() }],
            ..Default::default()
        });
        let map = TempoMap::from_song(&song);
        for beat in [64.0, 100.0, 200.0] {
            let table = map.beat_to_samples(beat, 48_000) as f64;
            let integrated = crate::automation::beats_to_samples(&song, 48_000, beat) as f64;
            assert!((table - integrated).abs() < 480.0, "beat {beat}: table {table} / integrated {integrated}");
        }
    }

    #[test]
    fn huge_beats_saturate_without_panicking() {
        let m = TempoMap::from_song(&const_song(120.0, 4.0));
        assert_eq!(m.beat_to_samples(f64::MAX, 48_000), u64::MAX);
        assert_eq!(m.beat_to_samples(f64::INFINITY, 48_000), u64::MAX);
    }

    #[test]
    fn varies_covers_curve_and_tempo_modulation() {
        assert!(!TempoMap::from_song(&const_song(120.0, 4.0)).varies());
        assert!(TempoMap::from_song(&ramp_song(120.0, 60.0, 4.0)).varies());

        let mut modulated = const_song(120.0, 4.0);
        modulated.song_mod_routings.push(crate::model::ModRouting {
            id: 1,
            target: AutomationTarget::SongTempo,
            source_id: 1,
            depth: 0.2,
            polarity: crate::model::Polarity::Unipolar,
            enabled: true,
        });
        let map = TempoMap::from_song(&modulated);
        assert!(map.varies() && !map.is_automated());
    }

    #[test]
    fn zero_and_negative_clamp() {
        let m = TempoMap::from_song(&const_song(120.0, 4.0));
        assert_eq!(m.beat_to_seconds(0.0), 0.0);
        assert_eq!(m.beat_to_seconds(-1.0), 0.0);
        assert_eq!(m.seconds_to_beat(0.0), 0.0);
        assert_eq!(m.seconds_to_beat(-5.0), 0.0);
    }
}
