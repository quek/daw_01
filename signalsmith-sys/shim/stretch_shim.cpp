#include "stretch_shim.h"

#include "signalsmith-stretch.h"

#include <cstddef>
#include <cstdint>
#include <new>
#include <vector>

#ifdef DAW01_SMS_COUNT_ALLOCS
#include <cstdlib>

// Replace the global operator new / delete so C++ heap traffic is countable.
//
// Rust's `#[global_allocator]` hook (assert_no_alloc) only sees Rust's
// GlobalAlloc; the vendored engine's std::vector growth goes straight to the
// CRT and is invisible to it. Without this, an RT regression inside the
// engine (a resize past the warm-up high-water mark) would leave the
// no-alloc test green. Compiled in only under the `alloc-count` cargo
// feature, so production builds are untouched.
//
// The replacements only count and forward — behaviour is unchanged. Sized and
// array forms are provided so the STL's calls land here too; the nothrow and
// over-aligned forms are left to the library (their defaults call these, resp.
// use _aligned_malloc, and the engine holds no over-aligned types).
//
// The counter is **per thread**: what an RT check cares about is "did anything
// allocate on the audio thread while it was rendering", and a process-wide
// counter would also pick up whatever other threads happen to be doing (which
// makes it useless under a parallel test runner). Plain storage, no atomics —
// each thread only ever touches its own.
namespace {
thread_local unsigned long long g_alloc_count = 0;
}

void *operator new(std::size_t size) {
    ++g_alloc_count;
    void *p = std::malloc(size != 0 ? size : 1);
    if (p == nullptr) {
        throw std::bad_alloc();
    }
    return p;
}
void *operator new[](std::size_t size) { return ::operator new(size); }
void operator delete(void *p) noexcept { std::free(p); }
void operator delete[](void *p) noexcept { std::free(p); }
void operator delete(void *p, std::size_t) noexcept { std::free(p); }
void operator delete[](void *p, std::size_t) noexcept { std::free(p); }
#endif

namespace {

/// Duck-typed channel accessor pairs the engine template expects
/// (`inputs[channel][frame]`), backed by two raw planes.
struct StereoIn {
    const float *ch[2];
    const float *operator[](int c) const { return ch[c]; }
};
struct StereoOut {
    float *ch[2];
    float *operator[](int c) const { return ch[c]; }
};

constexpr int CHANNELS = 2;

/// Seed for every stream start. Any fixed value works; this one is arbitrary.
constexpr std::uint32_t STREAM_SEED = 0x5EED0040u;

/// Where the next-constructed `ShimRandom` finds its state. The engine holds
/// its RNG as a *private* member, so the only way to reach it later is to make
/// the RNG itself point at storage we own — this thread-local hands that
/// storage over during construction (see `sms_stretch`'s ctor).
thread_local std::uint32_t *g_rng_slot = nullptr;

/// The engine's random source, supplied as its `RandomEngine` template
/// parameter, with its state kept **outside** the engine so the shim can
/// re-seed it at every stream start (`sms_reseed`).
///
/// Why this matters: the engine only randomises phases above 2x stretch
/// (`maxCleanStretch`), and `reset()` / `outputSeek()` do NOT rewind the RNG.
/// Engines are pooled and reused, so without an explicit re-seed the random
/// sequence position depends on **how much audio that pool entry happened to
/// have processed before** — live playback (which keeps its pool for the whole
/// session and depends on where you started / how many times you looped) would
/// then not match an offline export (which starts from fresh engines). Same
/// clip, different phase smear, only reproducible as "the WAV sounds different
/// from what I heard".
///
/// Using our own generator (rather than `std::default_random_engine`) also
/// makes the output identical across standard-library implementations.
struct ShimRandom {
    using result_type = std::uint32_t;
    std::uint32_t *state;

    explicit ShimRandom(long seed) : state(g_rng_slot) {
        if (state != nullptr) {
            // xorshift32 requires non-zero state.
            *state = static_cast<std::uint32_t>(seed) | 1u;
        }
    }
    static constexpr result_type min() { return 0; }
    static constexpr result_type max() { return 0xFFFFFFFFu; }
    result_type operator()() {
        std::uint32_t x = (state != nullptr) ? *state : STREAM_SEED;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        if (state != nullptr) {
            *state = x;
        }
        return x;
    }
};

} // namespace

struct sms_stretch {
    // Declared before `engine` so its address is valid when the engine's
    // constructor (and therefore `ShimRandom`'s) runs.
    std::uint32_t rng_state;
    signalsmith::stretch::SignalsmithStretch<float, ShimRandom> engine;

    sms_stretch()
        : rng_state(STREAM_SEED),
          // The comma expression publishes the RNG storage before the engine's
          // constructor consumes it.
          engine((g_rng_slot = &rng_state, static_cast<long>(STREAM_SEED))) {}
};

namespace {

/// Drive every internal `std::vector` to its high-water mark **off** the audio
/// thread. `configure()` sizes most of them, but `peaks` is only `reserve`d to
/// `bands/2` and grown by `emplace_back` inside `findPeaks()`; white noise
/// produces a maximally jagged spectrum, so this is the shape that makes it
/// grow. Without this, the first loud transposed block on the RT thread could
/// hit a reallocation. Also exercises `reset` / `output_seek` / `process`
/// so their internal `resize` calls settle at capacity.
void warm_up(sms_stretch *s, float sample_rate) {
    auto &engine = s->engine;
    const int block = engine.blockSamples();
    const int interval = engine.intervalSamples();
    const std::size_t chunk = static_cast<std::size_t>(block + interval);

    std::vector<float> noise_l(chunk), noise_r(chunk);
    std::vector<float> out_l(chunk), out_r(chunk);
    // xorshift32 — no <random> state, identical on every platform.
    std::uint32_t rng = 0x1234'5678u;
    auto next = [&rng]() {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        return static_cast<float>(static_cast<std::int32_t>(rng)) * 4.6566129e-10f;
    };
    for (std::size_t i = 0; i < chunk; ++i) {
        noise_l[i] = next();
        noise_r[i] = next();
    }

    // Both a frequency map (transpose != 1) and a formant shift, so the peak
    // finder and the formant analysis both run during the warm-up.
    engine.setTransposeSemitones(3.0f, 8000.0f / sample_rate);
    engine.setFormantSemitones(2.0f, true);

    StereoIn in{{noise_l.data(), noise_r.data()}};
    StereoOut out{{out_l.data(), out_r.data()}};

    const int seek_len = engine.outputSeekLength(1.0f);
    if (seek_len > 0 && static_cast<std::size_t>(seek_len) <= chunk) {
        engine.outputSeek(in, seek_len);
    }
    // Several passes so multiple block boundaries (and therefore multiple
    // `findPeaks` / formant passes) happen.
    for (int pass = 0; pass < 4; ++pass) {
        engine.process(in, static_cast<int>(chunk), out, static_cast<int>(chunk));
    }

    engine.setTransposeSemitones(0.0f, 0.0f);
    engine.setFormantSemitones(0.0f, false);
    engine.reset();
}

} // namespace

extern "C" {

sms_stretch *sms_create(float sample_rate) {
    if (!(sample_rate > 0.0f)) {
        return nullptr;
    }
    sms_stretch *s = nullptr;
    try {
        s = new sms_stretch();
        // `splitComputation` spreads each spectral block's work across the
        // samples of one interval instead of spiking on a single sample —
        // the right trade for an audio callback (it costs one extra interval
        // of output latency, which the caller compensates for anyway).
        s->engine.presetDefault(CHANNELS, sample_rate, /*splitComputation=*/true);
        warm_up(s, sample_rate);
    } catch (...) {
        delete s;
        return nullptr;
    }
    return s;
}

void sms_destroy(sms_stretch *s) { delete s; }

void sms_reset(sms_stretch *s) {
    if (s) {
        s->engine.reset();
    }
}

void sms_reseed(sms_stretch *s) {
    if (s) {
        s->rng_state = STREAM_SEED;
    }
}

int sms_input_latency(const sms_stretch *s) {
    return s ? s->engine.inputLatency() : 0;
}

int sms_output_latency(const sms_stretch *s) {
    return s ? s->engine.outputLatency() : 0;
}

void sms_set_transpose_semitones(sms_stretch *s, float semitones, float tonality_limit) {
    if (s) {
        s->engine.setTransposeSemitones(semitones, tonality_limit);
    }
}

void sms_set_formant_semitones(sms_stretch *s, float semitones, int compensate_pitch) {
    if (s) {
        s->engine.setFormantSemitones(semitones, compensate_pitch != 0);
    }
}

int sms_output_seek_length(const sms_stretch *s, double playback_rate) {
    return s ? s->engine.outputSeekLength(static_cast<float>(playback_rate)) : 0;
}

void sms_output_seek(sms_stretch *s, const float *in_l, const float *in_r, int n) {
    if (!s || !in_l || !in_r || n <= 0) {
        return;
    }
    // Every stream start begins from the same RNG position, so a clip sounds
    // the same no matter what that pooled engine processed before (live vs
    // offline export, first play vs after a loop).
    s->rng_state = STREAM_SEED;
    StereoIn in{{in_l, in_r}};
    s->engine.outputSeek(in, n);
}

void sms_process(sms_stretch *s, const float *in_l, const float *in_r, int in_n,
                 float *out_l, float *out_r, int out_n) {
    if (!s || !in_l || !in_r || !out_l || !out_r || out_n <= 0 || in_n < 0) {
        return;
    }
    StereoIn in{{in_l, in_r}};
    StereoOut out{{out_l, out_r}};
    s->engine.process(in, in_n, out, out_n);
}

unsigned long long sms_alloc_count(void) {
#ifdef DAW01_SMS_COUNT_ALLOCS
    return g_alloc_count;
#else
    return ~0ULL;
#endif
}

} // extern "C"
