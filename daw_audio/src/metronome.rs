//! メトロノーム click の生成 (Phase 7 B3、engine.rs から独立 module 化 —
//! `docs/plan_arch_refactor.md` §5)。monitoring 専用: live 再生と count-in
//! preroll だけが呼び、offline export (`render_master_buffer`) は通さない。

/// メトロノーム click voice (sine + linear envelope decay)。 1 voice mono。
/// beat 境界で trigger され、 `samples_remaining` が 0 になるまで sine を
/// mix。 連続 beat で前 voice が decay 中なら新 voice で overwrite (= 短
/// decay の業界標準 idiom)。
///
/// パラメータ default: decay 40 ms / amplitude peak 0.25 (-12 dB) / freq
/// downbeat 880 Hz 他 440 Hz。 すべて `render_metronome` で hardcode。
pub struct ClickVoice {
    /// remaining samples until envelope reaches 0 (= voice expires)
    pub samples_remaining: u32,
    /// total decay length in samples (envelope = remaining / decay_total)
    pub decay_samples: u32,
    /// oscillator frequency (Hz)
    pub freq: f32,
    /// phase accumulator (radians, 0..=TAU)
    pub phase: f32,
    /// 次 buffer 内で voice を再開する sample offset (= trigger frame)。
    /// 0 なら buffer の最初から再開 (= 前 buffer から続いている voice)。
    pub start_offset: u32,
}

/// メトロノーム click を 1 buffer 分 master_l/r に重ねる。 buffer 範囲内の
/// 全 beat 境界 (bpm + sample_rate から算出) で click voice を trigger、
/// 既存 voice が decay 中なら overwrite (= 短 decay 1 voice の業界標準
/// idiom)。 voice の sample 生成は sine + linear envelope decay で hardcode
/// (decay 40 ms / amp peak 0.25 = -12 dB / freq downbeat 880 Hz, 他 440 Hz)。
/// stereo は同 sample を L/R に均等 mix (= mono click)。
///
/// `playhead_samples` は **符号付き**: r.md #39 で PDC 補償 (= master に届く音の
/// 遅延ぶん click も遅らせる) を入れたため、曲頭付近では負の位置を取る。負の間は
/// beat 境界を跨がないので click は鳴らず、position 0 を含む buffer で 1 回だけ
/// 鳴る (`u64` の saturating clamp だと曲頭で beat 0 を毎 buffer 再 trigger して
/// しまうため、ここは必ず符号付きで計算する)。
///
/// RT 安全: heap 確保なし、 浮動小数演算と sin() 呼び出しのみ。 bpm = 0 /
/// sample_rate = 0 / tsig_num < 1 で no-op (defensive)。
///
/// 同 buffer 内に 2 個以上 beat 境界が含まれる場合 (= 高速 tempo / 大 buffer)、
/// 後の trigger が voice を overwrite し前の voice の残響は失われる。 通常
/// 使用範囲 (~600 BPM @ 11.6 ms buffer = 1.16 beat/buffer) では起きない。
#[allow(clippy::too_many_arguments)]
pub fn render_metronome(
    voice: &mut Option<ClickVoice>,
    master_l: &mut [f32],
    master_r: &mut [f32],
    frames: usize,
    playhead_samples: i64,
    sample_rate: u32,
    bpm: f32,
    tsig_num: i64,
) {
    if frames == 0 || sample_rate == 0 || bpm <= 0.0 || tsig_num < 1 {
        return;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
    if samples_per_beat <= 0.0 {
        return;
    }
    let buffer_start = playhead_samples as f64;
    let buffer_end = buffer_start + frames as f64;
    // この buffer 内に含まれる beat 境界 (= sample 位置 = beat_index *
    // samples_per_beat) を順次 trigger。 連続なら最後の trigger が voice を
    // overwrite (KISS: 同 buffer 多重 voice なし)。
    let first_beat_in_buf = (buffer_start / samples_per_beat).ceil() as i64;
    let mut beat_index = first_beat_in_buf.max(0);
    loop {
        let boundary_sample = beat_index as f64 * samples_per_beat;
        if boundary_sample >= buffer_end {
            break;
        }
        if boundary_sample >= buffer_start {
            let buf_offset = (boundary_sample - buffer_start).floor() as u32;
            if (buf_offset as usize) < frames {
                let downbeat = beat_index.rem_euclid(tsig_num) == 0;
                let decay = ((sample_rate as f32) * 0.04) as u32;
                let freq = if downbeat { 880.0 } else { 440.0 };
                *voice = Some(ClickVoice {
                    samples_remaining: decay.max(1),
                    decay_samples: decay.max(1),
                    freq,
                    phase: 0.0,
                    start_offset: buf_offset,
                });
            }
        }
        beat_index += 1;
    }
    // active voice の sample 生成 + mix。 start_offset から frames 末まで
    // sine + linear envelope decay。 voice 終端で None に戻す。
    if let Some(v) = voice.as_mut() {
        let mut i = v.start_offset as usize;
        v.start_offset = 0;
        let two_pi = std::f32::consts::TAU;
        let amp_peak: f32 = 0.25;
        let freq_per_sr = v.freq / sample_rate as f32;
        while i < frames && v.samples_remaining > 0 {
            let env = v.samples_remaining as f32 / v.decay_samples as f32;
            let s = v.phase.sin() * env * amp_peak;
            master_l[i] += s;
            master_r[i] += s;
            v.phase += two_pi * freq_per_sr;
            if v.phase > two_pi {
                v.phase -= two_pi;
            }
            v.samples_remaining -= 1;
            i += 1;
        }
        if v.samples_remaining == 0 {
            *voice = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const BPM: f32 = 120.0; // → 1 拍 = 24 000 sample

    /// 1 buffer 描画して L/R を返す。
    fn render_at(
        voice: &mut Option<ClickVoice>,
        pos: i64,
        frames: usize,
    ) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0f32; frames];
        let mut r = vec![0.0f32; frames];
        render_metronome(voice, &mut l, &mut r, frames, pos, SR, BPM, 4);
        (l, r)
    }

    /// 区間の絶対値和 (= その範囲に click が乗っているか)。
    fn energy(buf: &[f32], range: std::ops::Range<usize>) -> f32 {
        buf[range].iter().map(|v| v.abs()).sum()
    }

    #[test]
    fn click_starts_exactly_at_the_beat_boundary() {
        let mut v = None;
        // 拍 1 の境界 = 24 000 sample → buffer [23 900, 24 412) の offset 100。
        let (l, r) = render_at(&mut v, 23_900, 512);
        assert_eq!(energy(&l, 0..100), 0.0, "境界より前は無音");
        assert!(energy(&l, 100..512) > 0.0, "境界から click が鳴る");
        assert_eq!(l, r, "mono click を L/R 均等に");
    }

    #[test]
    fn negative_position_is_silent_and_beat_zero_fires_once() {
        // r.md #39: PDC 補償で click の参照位置は曲頭付近で負になる。負の間は拍境界を
        // 跨がないので無音、0 を含む buffer で 1 拍目が 1 回だけ鳴る。u64 の 0 クランプ
        // だと負の buffer 全部が「境界 0 を含む」と誤判定して毎 buffer 再 trigger する。
        let mut v = None;
        let mut start = -5_120i64;
        while start < 0 {
            let (l, _) = render_at(&mut v, start, 512);
            assert_eq!(energy(&l, 0..512), 0.0, "start={start} は拍境界を跨がない");
            assert!(v.is_none(), "start={start} で voice が立ってはいけない");
            start += 512;
        }
        let (l, _) = render_at(&mut v, 0, 512);
        assert!(energy(&l, 0..512) > 0.0, "曲頭 (位置 0) で 1 拍目が鳴る");
    }

    #[test]
    fn pdc_compensation_shifts_the_click_later_by_the_same_amount() {
        // 参照位置を latency 分だけ手前にする = buffer 内で同じだけ遅れて鳴る
        // (= 遅延プラグインを通った track の音と揃う)。
        let mut plain = None;
        let (l0, _) = render_at(&mut plain, 24_000, 512);
        assert!(energy(&l0, 0..512) > 0.0, "補償なしは buffer 先頭で鳴る");

        let mut compensated = None;
        let (l1, _) = render_at(&mut compensated, 24_000 - 200, 512);
        assert_eq!(energy(&l1, 0..200), 0.0, "補償量ぶん遅れる");
        assert!(energy(&l1, 200..512) > 0.0);
    }
}
