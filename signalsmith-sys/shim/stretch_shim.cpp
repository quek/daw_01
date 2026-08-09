#include "stretch_shim.h"

#include "signalsmith-stretch.h"

#include <cstddef>
#include <cstdint>
#include <new>
#include <vector>

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

} // namespace

struct sms_stretch {
    signalsmith::stretch::SignalsmithStretch<float> engine;

    // Deterministic seed: two engines given the same input must produce the
    // same output, otherwise offline export would not match live playback.
    // (`SignalsmithStretch()` seeds from `std::random_device`.)
    sms_stretch() : engine(/*seed=*/0x5EED'0040L) {}
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

} // extern "C"
