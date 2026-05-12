//! Audio clip renderer.
//!
//! Defines the data structures the live audio thread reads every buffer
//! to mix audio events into per-track scratch buffers. PR2 stood up
//! the types + an empty default so `EngineShared::audio_clip_renderer`
//! has a wait-free snapshot to `load()` from day one; PR6 added the
//! schedule compiler + Raw / Repitch render loop on top.
//!
//! VOICEVOX vocal output (the old `vocal.rs` / `VocalAudio`) shares the
//! same `AudioSourceBuffer` shape — PR8 routed it through
//! `MainToChild::SetGeneratedAudio` →
//! `EngineShared::generated_audio_store`, keyed by
//! `vocal_gen_id(track_id, clip_id)`. The actual per-clip vocal mix
//! still lives in `engine::process_track_owned`'s vocal block (Vocal
//! clips are MIDI-shaped with lyrics, so they don't appear in
//! `AudioContent` and thus aren't picked up by `compile_audio_schedule`
//! / `render_audio_events`); a future PR can migrate them onto
//! `AudioContent::Audio` once the model can express
//! "MIDI-with-baked-audio" cleanly.
//!
//! Spec: `docs/plan_audio_clip.md` §6 / §9.3.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use common::audio_render::{fade_envelope, pitch_ratio_for};
use common::model::{
    AudioSourceId, AudioSourcePath, ClipContent, FadeCurve, Song, StretchMode,
};

/// Decoded sample buffer for a single `AudioSource`. Planar storage
/// (`samples[channel][frame_idx]`). Shared via `Arc` between the IPC
/// receive loop, the decode worker, and the audio render thread —
/// the audio thread only ever clones the `Arc`, never the bytes.
pub struct AudioSourceBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

impl AudioSourceBuffer {
    /// Empty silent buffer — used as a placeholder when the source is
    /// missing or still decoding. Allocates `frames` zeros per channel.
    pub fn silent(sample_rate: u32, channels: u16, frames: u64) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            sample_rate,
            channels,
            frames,
            samples: (0..ch).map(|_| vec![0.0; frames as usize]).collect(),
        }
    }
}

/// One playable event flattened from an `AudioEvent` for the render
/// loop。
///
/// Phase 5 follow-up (audio clip tempo follow): beat-domain で trigger /
/// end / fade を保持し、 render loop で `playhead_beats` + `current_bpm`
/// から per-buffer sample 換算する。 これにより SongTempo curve に追随して
/// audio clip が:
/// - **trigger 位置が beat 単位で固定** (= 過去 tempo 履歴に依存しない)
/// - **Repitch mode: 再生速度が tempo 比 (= current_bpm/nominal_bpm) で
///   スケール**、 pitch も同時に変わる (vinyl 流)
/// - **Raw / Stretch / Slice mode: 再生速度は固定**、 ただし beat-domain
///   end で clip がカットされる場合あり (= raw 仕様、 stretch 実装は別 phase)
pub struct RenderedEvent {
    pub track_idx: usize,
    pub clip_idx: usize,
    /// First song-beat this event contributes audio at。
    pub start_beat: f64,
    /// Exclusive end song-beat。
    pub end_beat: f64,
    pub source_id: AudioSourceId,
    pub source_start_frames: u64,
    pub source_end_frames: u64,
    pub gain_lin: f32,
    pub pan: f32,
    /// Source frame stride per output frame at **nominal** bpm (= compile
    /// 時点の `song.bpm` での pitch ratio。 Repitch mode は per-buffer に
    /// `pitch_ratio * current_bpm / nominal_bpm` でスケール、 他 mode は
    /// 不変)。 1.0 = same speed as engine SR at nominal bpm。
    pub pitch_ratio: f64,
    /// compile 時に使われた base bpm。 Repitch mode の tempo ratio (= current
    /// / nominal) 計算に使う。 SongTempo curve が無い song は `song.bpm` と
    /// 一致するが、 ある song でも nominal は constant `song.bpm` (= base)。
    pub nominal_bpm: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
    pub reversed: bool,
    pub stretch_mode: StretchMode,
}

/// Wait-free snapshot of "what audio events should the audio thread
/// mix on the next buffer." Built off the audio thread (in
/// `compile_audio_schedule` — PR6) and published via `ArcSwap`. The
/// audio thread `load()`s a snapshot and reads it for the duration of
/// one buffer; new edits land via `store()` on the next callback.
pub struct AudioClipRenderer {
    /// Sorted by `start_frame` ascending. PR6's render loop bisects
    /// here to find events overlapping the current buffer.
    pub schedule: Vec<RenderedEvent>,
    /// `AudioSourceId → decoded buffer`. The render loop clones the
    /// `Arc` once per active event — no hashmap lookup beyond that.
    pub sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>>,
}

impl AudioClipRenderer {
    pub fn empty() -> Self {
        Self {
            schedule: Vec::new(),
            sources: HashMap::new(),
        }
    }
}

impl Default for AudioClipRenderer {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Phase 1 PR6: schedule compilation + WAV decode + render loop
// ---------------------------------------------------------------------------

/// Decode a WAV file into a planar `AudioSourceBuffer`. Mirror of
/// `daw_gui::import_audio::decode_wav` (Phase 1 keeps the two crates'
/// decoders independent so file-backed sources are decoded twice — once
/// per process — without IPC for sample data, see §6.1 / §8.3).
pub fn decode_wav(path: &Path) -> Result<AudioSourceBuffer> {
    use hound::SampleFormat;

    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("open wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        anyhow::bail!("{}: channels = 0", path.display());
    }
    if spec.sample_rate == 0 {
        anyhow::bail!("{}: sample_rate = 0", path.display());
    }
    let frames = reader.duration() as u64;
    let channels = spec.channels as usize;

    let mut planar: Vec<Vec<f32>> = (0..channels)
        .map(|_| Vec::with_capacity(frames as usize))
        .collect();
    match spec.sample_format {
        SampleFormat::Float => {
            for (idx, sample) in reader.samples::<f32>().enumerate() {
                let s = sample.with_context(|| format!("read f32 {}", path.display()))?;
                planar[idx % channels].push(s);
            }
        }
        SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            for (idx, sample) in reader.samples::<i32>().enumerate() {
                let s = sample.with_context(|| format!("read i32 {}", path.display()))?;
                planar[idx % channels].push(s as f32 / max_val);
            }
        }
    }
    Ok(AudioSourceBuffer {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames,
        samples: planar,
    })
}

/// Build an `AudioClipRenderer` snapshot from the current Song. Walks
/// `Song.audio_sources` (decoding file-backed entries via `hound`), then
/// flattens every `ClipContent::Audio` event in every track into the
/// schedule. Sorted by `start_frame` ascending so the render loop can
/// short-circuit once `start_frame >= buf_end`.
///
/// Decode is synchronous on the caller (the IPC receive loop in Phase 1).
/// Phase 2 moves decode to a background thread so >100 MB samples don't
/// stall the receive loop.
///
/// PR-V4: `AudioSourcePath::Generated` 経路 (= 旧 VOICEVOX `SetGenerated
/// Audio` 経由で渡される generated buffer の参照) は廃止。 VOICEVOX 合成
/// は builtin instrument plugin (`PluginFormat::Builtin`) 内で完結する。
/// 既存 project が `AudioSourcePath::Generated` を含んで読まれた場合は
/// warn ログ + skip (= silent な audio として再生される)。
pub fn compile_audio_schedule(
    song: &Song,
    project_dir: Option<&Path>,
    engine_sample_rate: u32,
) -> AudioClipRenderer {
    let mut sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>> = HashMap::new();
    if engine_sample_rate == 0 || song.bpm <= 0.0 {
        return AudioClipRenderer::empty();
    }
    // Phase 5 follow-up (audio clip tempo follow): schedule は beat-domain で
    // 保持するので、 compile-time に samples_per_beat 換算は不要。 fade /
    // 範囲は beat のまま、 nominal_bpm = song.bpm を per-event に控える。

    // -- Resolve every AudioSource into a decoded buffer (or skip on error) --
    for (&id, source) in &song.audio_sources {
        let buffer = match &source.path {
            AudioSourcePath::ProjectRelative(rel) => {
                let Some(dir) = project_dir else {
                    tracing::warn!(?rel, "ProjectRelative source but project_dir is unset; skipping");
                    continue;
                };
                let abs = dir.join(rel);
                match decode_wav(&abs) {
                    Ok(buf) => Arc::new(buf),
                    Err(e) => {
                        tracing::error!(error = ?e, path = %abs.display(), "decode failed");
                        continue;
                    }
                }
            }
            AudioSourcePath::Absolute(abs) => match decode_wav(abs) {
                Ok(buf) => Arc::new(buf),
                Err(e) => {
                    tracing::error!(error = ?e, path = %abs.display(), "decode failed");
                    continue;
                }
            },
            AudioSourcePath::Generated { id: gen_id } => {
                tracing::warn!(
                    gen_id,
                    "PR-V4: AudioSourcePath::Generated は廃止 (VOICEVOX は builtin plugin 経由)、 skipping"
                );
                continue;
            }
        };
        sources.insert(id, buffer);
    }

    // -- Flatten every audio clip's events into RenderedEvent ----------------
    let mut schedule: Vec<RenderedEvent> = Vec::new();
    for (track_idx, track) in song.tracks.iter().enumerate() {
        for (clip_idx, clip) in track.clips.iter().enumerate() {
            let Some(content) = song.clip_contents.get(&clip.content_id) else {
                continue;
            };
            let ClipContent::Audio(audio) = content else {
                continue;
            };
            for event in &audio.events {
                let Some(buffer) = sources.get(&event.source_id) else {
                    continue;
                };
                let event_start_beat = clip.start_beat + event.event_start_in_clip_beats;
                let event_end_beat = event_start_beat + event.event_length_beats;
                if event_end_beat <= event_start_beat {
                    continue;
                }
                let pitch_ratio = pitch_ratio_for(
                    event.stretch_mode,
                    buffer.sample_rate,
                    engine_sample_rate,
                    event.pitch_semitones,
                );
                let gain_lin = 10f32.powf(event.gain_db / 20.0);
                if event.muted {
                    continue;
                }
                schedule.push(RenderedEvent {
                    track_idx,
                    clip_idx,
                    start_beat: event_start_beat,
                    end_beat: event_end_beat,
                    source_id: event.source_id,
                    source_start_frames: event.source_start_frames,
                    source_end_frames: event.source_end_frames,
                    gain_lin,
                    pan: event.pan.clamp(-1.0, 1.0),
                    pitch_ratio,
                    nominal_bpm: song.bpm,
                    fade_in_beats: event.fade_in_beats.max(0.0),
                    fade_out_beats: event.fade_out_beats.max(0.0),
                    fade_in_curve: event.fade_in_curve,
                    fade_out_curve: event.fade_out_curve,
                    reversed: event.reversed,
                    stretch_mode: event.stretch_mode,
                });
            }
        }
    }
    schedule.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    tracing::info!(
        n_events = schedule.len(),
        n_sources = sources.len(),
        engine_sr = engine_sample_rate,
        bpm = song.bpm,
        "compiled audio schedule"
    );
    AudioClipRenderer { schedule, sources }
}

/// Mix every audio event for `track_idx` into `track_l/track_r` for the
/// frame range `[playhead .. playhead+frames)`. Called from
/// `process_track_owned` after the track buffers are zeroed and before
/// the audio FX chain. Adds (`+=`) to the existing buffer so the
/// instrument plugin's audio output is preserved (= Bitwig Hybrid Track:
/// audio clip output bypasses the instrument and joins the FX chain
/// input alongside it, see §13 Q6).
#[allow(clippy::too_many_arguments)]
pub fn render_audio_events(
    renderer: &AudioClipRenderer,
    track_idx: usize,
    track_l: &mut [f32],
    track_r: &mut [f32],
    playhead_beats: f64,
    current_bpm: f32,
    sample_rate: u32,
    frames: u32,
) {
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let n = frames as usize;
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    let buf_end_beats =
        playhead_beats + f64::from(frames) / samples_per_beat;

    for event in &renderer.schedule {
        if event.track_idx != track_idx {
            continue;
        }
        // schedule is sorted by start_beat ascending; early-out once we
        // pass the buffer end.
        if event.start_beat >= buf_end_beats {
            break;
        }
        if event.end_beat <= playhead_beats {
            continue;
        }
        let Some(buffer) = renderer.sources.get(&event.source_id) else {
            continue;
        };
        if buffer.samples.is_empty() {
            continue;
        }

        // Compute the beat-domain overlap of [event.start_beat, event.end_beat)
        // with the buffer's beat range, then convert to per-buffer sample
        // offsets using `samples_per_beat` (= current_bpm based)。
        let render_start_beat = event.start_beat.max(playhead_beats);
        let render_end_beat = event.end_beat.min(buf_end_beats);
        if render_end_beat <= render_start_beat {
            continue;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let buf_off_start =
            ((render_start_beat - playhead_beats) * samples_per_beat).max(0.0) as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let buf_off_end_raw =
            ((render_end_beat - playhead_beats) * samples_per_beat).max(0.0) as usize;
        let buf_off_end = buf_off_end_raw.min(n);
        if buf_off_end <= buf_off_start {
            continue;
        }

        // Phase 5 follow-up (audio clip tempo follow): Repitch mode は
        // tempo 比でレートをスケール (= vinyl 流)、 他 mode は不変。
        let tempo_ratio = if event.nominal_bpm > 0.0 {
            f64::from(current_bpm) / f64::from(event.nominal_bpm)
        } else {
            1.0
        };
        let effective_pitch_ratio = match event.stretch_mode {
            StretchMode::Repitch => event.pitch_ratio * tempo_ratio,
            _ => event.pitch_ratio,
        };

        // event_total_frames は元実装では「event の音域 sample 数」 として
        // fade envelope の tail 計算に使われていた。 beat-domain 化で
        // 「buffer 内 sample 数」 ベースに置き換えた fade を考えると、 event の
        // 「総 buffer 内 sample 数」 = `event 長 beats × samples_per_beat` だが、
        // fade 計算は per-buffer の event-local sample offset で完結するので
        // この値は不要。 削除 (= 元 sample-domain ロジックの再現に必要だった
        // tail 計算は per-sample の event_local + fade_*_samples で行う)。
        let fade_in_samples =
            (event.fade_in_beats * samples_per_beat).max(0.0) as u64;
        let fade_out_samples =
            (event.fade_out_beats * samples_per_beat).max(0.0) as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let event_total_samples = ((event.end_beat - event.start_beat)
            * samples_per_beat)
            .max(0.0) as u64;
        // event 開始からの absolute sample offset を求めるための起点 (= event
        // start を sample 単位で表現した値、 playhead_beats 基準)。
        let event_start_offset_in_buf =
            ((event.start_beat - playhead_beats) * samples_per_beat) as i64;
        let source_len = event
            .source_end_frames
            .saturating_sub(event.source_start_frames);

        // Channel layout: planar samples[ch][frame].
        let l_plane: &[f32] = buffer
            .samples
            .first()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let r_plane: &[f32] = if buffer.channels >= 2 {
            buffer
                .samples
                .get(1)
                .map(Vec::as_slice)
                .unwrap_or(l_plane)
        } else {
            l_plane
        };

        // Equal-power pan for mono → stereo / balance pan for stereo.
        // pan = 0 → no change; pan > 0 → right; pan < 0 → left.
        let pan_rad = (event.pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let pan_l = pan_rad.cos();
        let pan_r = pan_rad.sin();

        for i in buf_off_start..buf_off_end {
            // event_local = sample offset since event.start_beat。 i は buffer
            // 内 offset、 buffer 開始は playhead_beats、 event 開始は
            // event.start_beat に対応する buf 内 offset `event_start_offset_in_buf`。
            // よって `event_local = i - event_start_offset_in_buf` (= 負 / 範囲外なら skip)。
            let event_local_signed = i as i64 - event_start_offset_in_buf;
            if event_local_signed < 0 {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let event_local = event_local_signed as u64;

            // Fade envelope (in × out)。 beat-domain fade → samples 換算済の
            // fade_in_samples / fade_out_samples を per-sample 比較。
            let fade_in = fade_envelope(event_local, fade_in_samples, event.fade_in_curve);
            let tail = event_total_samples.saturating_sub(event_local + 1);
            let fade_out = fade_envelope(tail, fade_out_samples, event.fade_out_curve);
            let env = fade_in * fade_out * event.gain_lin;
            if env == 0.0 {
                continue;
            }

            // Source position with linear interpolation. effective_pitch_ratio
            // は Repitch mode で tempo ratio スケール済、 他 mode は不変。
            let source_pos = event_local as f64 * effective_pitch_ratio;
            let source_pos = if event.reversed {
                source_len as f64 - 1.0 - source_pos
            } else {
                source_pos
            };
            if source_pos < 0.0 {
                continue;
            }
            let i0 = source_pos.floor() as i64;
            let frac = (source_pos - i0 as f64) as f32;
            if i0 < 0 {
                continue;
            }
            let abs_idx0 = event.source_start_frames + i0 as u64;
            let abs_idx1 = abs_idx0 + 1;
            if abs_idx0 >= event.source_end_frames || abs_idx0 >= buffer.frames {
                continue;
            }
            let s_l0 = l_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
            let s_r0 = r_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
            let s_l1 = l_plane.get(abs_idx1 as usize).copied().unwrap_or(s_l0);
            let s_r1 = r_plane.get(abs_idx1 as usize).copied().unwrap_or(s_r0);
            let s_l = s_l0 + (s_l1 - s_l0) * frac;
            let s_r = s_r0 + (s_r1 - s_r0) * frac;

            track_l[i] += s_l * env * pan_l * std::f32::consts::SQRT_2;
            track_r[i] += s_r * env * pan_r * std::f32::consts::SQRT_2;
        }
    }
}
