// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

/*
 * C ABI shim over Signalsmith Stretch (vendor/signalsmith-stretch, MIT).
 *
 * The C++ engine is a template with `Inputs`/`Outputs` duck-typed channel
 * accessors; this shim pins it to `float` and a **fixed stereo layout** so the
 * Rust side never has to build a `*const *const f32` channel-pointer array per
 * call (that would be a heap allocation on the audio thread).
 *
 * Stereo is fixed rather than per-source because the engine pool in daw_audio
 * hands one slot to whichever event colours onto it; a mono source would
 * otherwise force a reconfigure (= allocation) when the slot is reused by a
 * stereo one. daw_audio's clip renderer is stereo end to end (it duplicates a
 * mono plane into both), so nothing is lost but CPU on mono material.
 *
 * Threading / RT contract:
 * - `sms_create` / `sms_destroy` allocate and MUST run off the audio thread.
 *   `sms_create` also runs a noise warm-up so every internal `std::vector`
 *   reaches its high-water mark before the RT thread ever calls in.
 * - `sms_reset` / `sms_output_seek` / `sms_process` / the setters are
 *   allocation-free after that warm-up and are the only calls the RT thread
 *   makes.
 * - A handle is owned by exactly one thread at a time (no internal locking).
 */
#ifndef DAW01_SIGNALSMITH_STRETCH_SHIM_H
#define DAW01_SIGNALSMITH_STRETCH_SHIM_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct sms_stretch sms_stretch;

/* Allocate + configure + warm up a stereo engine. Returns NULL on failure. */
sms_stretch *sms_create(float sample_rate);
void sms_destroy(sms_stretch *s);

/* Drop all stream state (spectra, overlap-add tail, input history). */
void sms_reset(sms_stretch *s);

/* Rewind the random source used for phase randomisation (which the engine
 * only engages above 2x stretch). `sms_output_seek` already does this, so a
 * stream always starts from the same RNG position regardless of what that
 * pooled engine processed before — that is what keeps an offline export
 * identical to what was heard live. */
void sms_reseed(sms_stretch *s);

/* Latency of the internal pipeline, in samples. See `sms_output_seek`. */
int sms_input_latency(const sms_stretch *s);
int sms_output_latency(const sms_stretch *s);

/* Pitch shift. `tonality_limit` is a frequency **relative to the sample rate**
 * (i.e. Hz / sample_rate); above it, frequencies are shifted less so the
 * material keeps its timbre. 0 disables the limit. */
void sms_set_transpose_semitones(sms_stretch *s, float semitones, float tonality_limit);

/* Spectral-envelope (formant) shift. `compensate_pitch != 0` keeps the
 * envelope where it was while the pitch moves — that is what makes a
 * transposed voice keep its character. */
void sms_set_formant_semitones(sms_stretch *s, float semitones, int compensate_pitch);

/* Fill the pipeline so that the *next* `sms_process` output sample is the
 * first sample of `in_*`. `n` must be `sms_output_seek_length(rate)`, where
 * `rate` is input samples consumed per output sample. */
int sms_output_seek_length(const sms_stretch *s, double playback_rate);
void sms_output_seek(sms_stretch *s, const float *in_l, const float *in_r, int n);

/* Consume `in_n` input samples and emit `out_n` output samples; the
 * time-stretch ratio is `in_n / out_n`. */
void sms_process(sms_stretch *s, const float *in_l, const float *in_r, int in_n,
                 float *out_l, float *out_r, int out_n);

/* Number of C++ heap allocations made so far **on the calling thread**.
 *
 * Only meaningful when built with the `alloc-count` cargo feature (which
 * defines DAW01_SMS_COUNT_ALLOCS and replaces the global operator new /
 * delete); otherwise this returns UINT64_MAX to mean "not instrumented".
 *
 * This exists because Rust's `#[global_allocator]` hook (assert_no_alloc)
 * cannot see C++ allocations at all: they go straight to the CRT. Without
 * it, a vendored-engine regression that reintroduces a realloc inside
 * process() would leave the RT no-alloc test green.
 *
 * Caveat: over-aligned allocations (operator new(size_t, align_val_t)) are
 * not replaced and therefore not counted. The engine's containers hold
 * float / std::complex<float> / plain structs, none of which are
 * over-aligned, so the vectors that can actually grow are all covered. */
unsigned long long sms_alloc_count(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* DAW01_SIGNALSMITH_STRETCH_SHIM_H */
