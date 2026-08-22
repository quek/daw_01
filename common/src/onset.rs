//! Onset (transient) detection for `StretchMode::Slice` (r.md #8 B1).
//!
//! [`AudioEvent::onsets`](crate::model::AudioEvent::onsets) holds the source
//! sample positions where each slice begins. The render path
//! (`slice_sample_at` in `daw_audio`) triggers one slice per onset and
//! time-stretches the gaps to follow tempo — but nothing populated `onsets`,
//! so Slice mode silently degraded to a single whole-source slice (= Raw).
//! This module is the missing detector.
//!
//! Method = **energy-flux** (no FFT): pre-emphasis high-pass to weight
//! transients, a per-hop energy envelope, a rectified first-difference onset
//! detection function (ODF), an adaptive (local-mean) threshold, local-peak
//! picking, and a minimum inter-onset interval to de-bounce. This is the
//! classic dependency-free approach for percussive loop slicing (Recycle /
//! Ableton Simpler). It runs OFF the RT path — it scans the whole source once
//! at analysis time (import / switch-to-Slice), never in the audio callback.

/// Hop (= ODF frame) length ≈ 5 ms, clamped so very low sample rates still
/// get a sane window. Non-overlapping hops keep the math O(n) and exact.
fn hop_len(sample_rate: u32) -> usize {
    (sample_rate as usize / 200).max(64)
}

/// Detect onset positions (mono sample indices) for slicing.
///
/// `samples` is the mono source (caller downmixes L/R). Returns a sorted,
/// de-duplicated list that **always starts at 0** (the first slice begins at
/// source start). Silent / too-short input → `[0]` (a single slice spanning
/// the whole source, i.e. Raw-equivalent playback).
///
/// `sensitivity` in `0.0..=1.0` scales the adaptive threshold: `0.5` is the
/// neutral default, higher finds more (quieter) onsets, lower finds fewer.
pub fn detect_onsets(samples: &[f32], sample_rate: u32, sensitivity: f32) -> Vec<u64> {
    let hop = hop_len(sample_rate);
    // Need at least a couple of hops to have a flux signal at all.
    if samples.len() < hop * 3 || sample_rate == 0 {
        return vec![0];
    }

    // 1. Per-hop energy of the pre-emphasised signal. Pre-emphasis is a
    //    one-tap difference `x[n] - x[n-1]` (a 6 dB/oct high-pass) that lifts
    //    transient/HF content over sustained low-frequency energy, so a
    //    bass note's body doesn't swamp a hi-hat's attack.
    let n_frames = samples.len() / hop;
    let mut energy = vec![0.0f32; n_frames];
    for (f, e) in energy.iter_mut().enumerate() {
        let start = f * hop;
        let mut acc = 0.0f32;
        for n in start..start + hop {
            // `n == 0` has no predecessor; treat x[-1] = 0.
            let prev = if n == 0 { 0.0 } else { samples[n - 1] };
            let d = samples[n] - prev;
            acc += d * d;
        }
        *e = acc;
    }

    // 2. Onset detection function = rectified first difference of the energy
    //    envelope (only rising edges = note attacks count).
    let mut odf = vec![0.0f32; n_frames];
    for i in 1..n_frames {
        odf[i] = (energy[i] - energy[i - 1]).max(0.0);
    }

    // 3. Adaptive threshold: local mean of the ODF over a ~±100 ms window,
    //    scaled by a sensitivity-derived multiplier, plus a small floor
    //    relative to the global peak so flat/near-silent regions never fire.
    //    sensitivity 0..1 → multiplier ~3.0 (few) .. ~1.05 (many); 0.5 → ~1.8.
    let half_win = (n_frames / 16).clamp(4, 64);
    let mult = 1.05 + (1.0 - sensitivity.clamp(0.0, 1.0)) * 1.95;
    let global_peak = odf.iter().copied().fold(0.0f32, f32::max);
    let floor = global_peak * 0.04;
    let min_gap_frames = {
        // ~30 ms minimum spacing between onsets (de-bounce double triggers).
        let g = (sample_rate as usize * 30 / 1000) / hop;
        g.max(1)
    };

    let mut onsets: Vec<u64> = vec![0];
    let mut last_frame: isize = -(min_gap_frames as isize); // allow an early second onset
    for i in 1..n_frames - 1 {
        let lo = i.saturating_sub(half_win);
        let hi = (i + half_win).min(n_frames);
        let window = &odf[lo..hi];
        let mean = window.iter().copied().sum::<f32>() / window.len() as f32;
        let thresh = (mean * mult).max(floor);
        // Local peak of the ODF above the adaptive threshold.
        let is_peak = odf[i] > thresh && odf[i] >= odf[i - 1] && odf[i] >= odf[i + 1];
        if is_peak && (i as isize - last_frame) >= min_gap_frames as isize {
            let pos = (i * hop) as u64;
            if pos != 0 {
                onsets.push(pos);
            }
            last_frame = i as isize;
        }
    }

    onsets.dedup();
    onsets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a mono signal of `len` samples with a short decaying-noise burst
    /// starting at each position in `bursts`. Deterministic (LCG, no rng dep).
    fn signal_with_bursts(len: usize, bursts: &[usize], sample_rate: u32) -> Vec<f32> {
        let mut s = vec![0.0f32; len];
        let burst_len = sample_rate as usize / 20; // 50 ms burst
        let mut state: u32 = 0x1234_5678;
        let mut rng = || {
            // xorshift-ish LCG → [-1, 1)
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        };
        for &b in bursts {
            for k in 0..burst_len {
                let idx = b + k;
                if idx >= len {
                    break;
                }
                // Sharp attack, exponential decay → a clear transient.
                let env = (-(k as f32) / (burst_len as f32 * 0.3)).exp();
                s[idx] += rng() * env;
            }
        }
        s
    }

    #[test]
    fn detects_bursts_at_known_positions() {
        let sr = 48_000;
        let bursts = [0usize, 12_000, 24_000, 36_000]; // every 0.25 s
        let sig = signal_with_bursts(48_000, &bursts, sr);
        let onsets = detect_onsets(&sig, sr, 0.5);

        // Always starts at 0.
        assert_eq!(onsets[0], 0, "first onset must be source start");
        // Found roughly one onset per burst (allow the detector to merge/miss
        // at most one — the point is it is not degenerate and not flooding).
        assert!(
            (bursts.len()..=bursts.len() + 1).contains(&onsets.len()),
            "expected ~{} onsets, got {onsets:?}",
            bursts.len()
        );
        // Each non-zero burst has a detected onset within 15 ms.
        let tol = sr as u64 * 15 / 1000;
        for &b in &bursts[1..] {
            let hit = onsets.iter().any(|&o| o.abs_diff(b as u64) <= tol);
            assert!(hit, "no onset near burst {b}; got {onsets:?}");
        }
    }

    #[test]
    fn silent_or_short_input_is_single_slice() {
        assert_eq!(detect_onsets(&[], 48_000, 0.5), vec![0]);
        assert_eq!(detect_onsets(&[0.0; 10], 48_000, 0.5), vec![0]);
        // Long pure silence → no flux → single slice.
        assert_eq!(detect_onsets(&vec![0.0; 48_000], 48_000, 0.5), vec![0]);
    }

    #[test]
    fn sensitivity_monotonic_in_onset_count() {
        let sr = 48_000;
        // Mix of strong and weak transients so threshold actually matters.
        let strong = [0usize, 16_000, 32_000];
        let mut sig = signal_with_bursts(48_000, &strong, sr);
        // Weak bursts halfway between the strong ones.
        for (i, w) in [8_000usize, 24_000, 40_000].iter().enumerate() {
            let weak = signal_with_bursts(48_000, &[*w], sr);
            for (d, v) in sig.iter_mut().zip(weak) {
                *d += v * (0.15 + 0.02 * i as f32);
            }
        }
        let few = detect_onsets(&sig, sr, 0.1).len();
        let many = detect_onsets(&sig, sr, 0.9).len();
        assert!(
            many >= few,
            "higher sensitivity must not find fewer onsets: few={few} many={many}"
        );
    }
}
