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
use common::audio_render::{fade_envelope, pitch_ratio_for, stretch_ratio_for, tempo_follow_ratio};
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
/// - **Repitch: 再生速度が tempo 比 (current_bpm/nominal_bpm) でスケール**、
///   pitch も同時に変わる (vinyl 流)
/// - **Stretch: granular time-stretch が tempo に追従** (pitch 保持、
///   `granular_sample_at`)
/// - **Slice: onset slicing が tempo に追従** (`slice_sample_at`、 onset 自動検出は
///   r.md #8 B1)
/// - **Raw: native rate 再生**、 BPM 変更時は clip 窓を秒固定で再スケール (r.md #7)
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
    /// source の native 長 / event の配置長 の比
    /// (= `native_secs / event_secs`、 nominal bpm 基準)。 `1.0` で「source を
    /// そのまま」 (= trim、 native rate)。 `< 1.0` で event slot の方が長い → source
    /// を引き伸ばす (slow)、 `> 1.0` で詰める (fast)。 Repitch は pitch にも乗る
    /// (tape)、 Stretch は granular で pitch 保持、 Slice は slice 配置にのみ作用、
    /// **Raw は無視** (= Raw は時間操作しない定義、 trim/cut)。 compile 時に
    /// off-RT で除算して確定し、 render loop では掛けるだけ (RT 安全)。
    pub stretch_ratio: f64,
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
    /// Phase 5 follow-up (StretchMode::Slice): source 内の transient sample
    /// 位置 (`AudioEvent.onsets` の clone)。 Slice mode の render path で、
    /// 各 slice は native rate で再生され、 slice の trigger 位置は
    /// `onsets[i] / tempo_ratio` で出力 sample 位置にマップされる。 Slice 以外
    /// の mode では参照されない。 通常 ~10..100 件の小さな Vec。
    pub onsets: Vec<u64>,
    /// Warp markers (`AudioEvent.beat_markers` の clone、 `locked_beat` 昇順・
    /// dedup 済)。 Stretch mode の granular render が `warp_source_frame` で
    /// 非一様タイムストレッチに使う (r.md #8 B12)。 < 2 件なら uniform stretch。
    pub beat_markers: Vec<common::model::BeatMarker>,
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
/// **Source reuse (r.md #7 decode 再設計 A):** `prev` is the live renderer
/// (or `None`). Any source already decoded there is `Arc`-cloned instead of
/// re-decoded, so a re-compile from a BPM change / edit / scrub does **zero**
/// WAV decoding and never stalls the caller.
///
/// **`decode_missing` (B):** when `true`, sources absent from `prev` are
/// decoded synchronously (used by the background decode worker and offline
/// export). When `false`, they are skipped — their events drop out of the
/// schedule (= momentarily silent) until the worker fills them in. This is the
/// fast, non-blocking path the IPC receive loop takes on `LoadSong`.
///
/// PR-V4: `AudioSourcePath::Generated` 経路 (= 旧 VOICEVOX `SetGenerated
/// Audio` 経由で渡される generated buffer の参照) は廃止。 VOICEVOX 合成
/// は builtin instrument plugin (`PluginFormat::Builtin`) 内で完結する。
/// 既存 project が `AudioSourcePath::Generated` を含んで読まれた場合は
/// warn ログ + skip (= silent な audio として再生される)。
pub fn compile_audio_schedule(
    song: &Song,
    prev: Option<&AudioClipRenderer>,
    project_dir: Option<&Path>,
    engine_sample_rate: u32,
    decode_missing: bool,
) -> AudioClipRenderer {
    let mut sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>> = HashMap::new();
    if engine_sample_rate == 0 || song.bpm <= 0.0 {
        return AudioClipRenderer::empty();
    }
    // Phase 5 follow-up (audio clip tempo follow): schedule は beat-domain で
    // 保持するので、 compile-time に samples_per_beat 換算は不要。 fade /
    // 範囲は beat のまま、 nominal_bpm = song.bpm を per-event に控える。

    // -- Resolve every AudioSource into a decoded buffer ----------------------
    for (&id, source) in &song.audio_sources {
        // (A) reuse an already-decoded buffer from the live renderer. Source ids
        //     are stable and never recycled, so a matching id is the same file —
        //     no re-decode needed (r.md #7 decode 再設計 A)。
        if let Some(prev) = prev
            && let Some(buf) = prev.sources.get(&id)
        {
            sources.insert(id, Arc::clone(buf));
            continue;
        }
        // (B) not cached: decode now only if asked. Otherwise leave it out — its
        //     events drop from the schedule until a later full compile fills it.
        if !decode_missing {
            continue;
        }
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
            // muted clip は全 audio event を schedule から除外する
            // (per-event `event.muted` とは独立。clip-level mute の SSoT)。
            if clip.muted {
                continue;
            }
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
                // clip time-stretch 量 = source native 長 / event 配置長
                // (秒で比較、 engine SR に依らない)。 nominal bpm 基準で固定し、
                // tempo-follow (current/nominal) とは render loop で乗算合成する。
                // trim では source 窓と event 長が lockstep するので比 ≈ 1.0。
                let stretch_ratio = stretch_ratio_for(
                    event.source_end_frames.saturating_sub(event.source_start_frames),
                    buffer.sample_rate,
                    event.event_length_beats,
                    song.bpm,
                );
                let gain_lin = 10f32.powf(event.gain_db / 20.0);
                if event.muted {
                    continue;
                }
                // Phase 5 follow-up (StretchMode::Slice) bug fix: onsets が
                // sort 済の不変条件は model に明示されておらず、 user / import
                // 経路次第で未 sort のまま入る可能性がある。 audio thread の
                // `slice_sample_at` は `partition_point` 前提で sorted を期待
                // するので、 compile 時 (off-RT) に一度 sort し直して保証する。
                let mut onsets_sorted = event.onsets.clone();
                onsets_sorted.sort_unstable();
                onsets_sorted.dedup();
                // B12 (r.md #8): warp markers を locked_beat 昇順 + dedup して保持
                // (warp_source_frame は sorted・non-degenerate を前提)。
                let mut warp_markers = event.beat_markers.clone();
                warp_markers.sort_by(|a, b| a.locked_beat.total_cmp(&b.locked_beat));
                warp_markers.dedup_by(|a, b| (a.locked_beat - b.locked_beat).abs() < 1e-9);
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
                    stretch_ratio,
                    nominal_bpm: song.bpm,
                    fade_in_beats: event.fade_in_beats.max(0.0),
                    fade_out_beats: event.fade_out_beats.max(0.0),
                    fade_in_curve: event.fade_in_curve,
                    fade_out_curve: event.fade_out_curve,
                    reversed: event.reversed,
                    stretch_mode: event.stretch_mode,
                    onsets: onsets_sorted,
                    beat_markers: warp_markers,
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

/// Does `song` reference any file-backed `AudioSource` that `renderer` has not
/// decoded yet? `true` ⇒ the background decode worker must run a full compile to
/// fill them in. `Generated` sources are excluded (never decoded here).
pub fn has_undecoded_sources(song: &Song, renderer: &AudioClipRenderer) -> bool {
    song.audio_sources.iter().any(|(id, source)| {
        !matches!(source.path, AudioSourcePath::Generated { .. })
            && !renderer.sources.contains_key(id)
    })
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
    // Phase 5 follow-up (granular DSP click 抑制) / r.md #6: LP smoothed な
    // **絶対** current_bpm (BPM 単位)。 audio thread が per-buffer に 1-pole LP
    // 更新した値で、 Stretch mode が `tempo_follow_ratio(stretch_ratio,
    // smoothed_current_bpm, nominal_bpm)` を計算するのに渡す。 Repitch / Raw /
    // Slice mode は instantaneous な `current_bpm` を使う (= pitch / slice
    // trigger の追随性を優先)。
    smoothed_current_bpm: f64,
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

        // Phase 5 follow-up (audio clip tempo follow) / r.md #6: source 進度 =
        // 手動 stretch_ratio × tempo 追従比 (current_bpm / nominal_bpm)。 この 2 つを
        // 掛けると clip は拍数固定のまま tempo に追従して伸縮する (= MIDI 流)。
        // Stretch (granular) は click 抑制のため smoothed bpm、 Repitch / Slice は
        // pitch / slice trigger の追随性優先で instant bpm を使う。 nominal_bpm は
        // per-event の compile時 song.bpm なので、 base bpm を変えても追従する
        // (= 旧実装の Stretch は current/song.bpm 駆動で、 song.bpm 自身が動くと
        // 比が 1.0 に戻り base bpm 変更に追従しなかった)。
        let nominal_bpm = f64::from(event.nominal_bpm);
        let follow_instant =
            tempo_follow_ratio(event.stretch_ratio, f64::from(current_bpm), nominal_bpm);
        let follow_smoothed =
            tempo_follow_ratio(event.stretch_ratio, smoothed_current_bpm, nominal_bpm);
        let effective_pitch_ratio = match event.stretch_mode {
            // Repitch (tape 式) は clip 長 stretch + tempo 追従が再生速度に乗る
            // (= pitch も一緒に変わる、 vinyl 流)。 Raw は stretch_ratio / tempo を
            // 無視 (= 時間操作しない定義、 native rate で trim/cut)。
            StretchMode::Repitch => event.pitch_ratio * follow_instant,
            _ => event.pitch_ratio,
        };

        // beat-domain fade を per-buffer の current_bpm で sample 換算する。
        // `event_total_samples` は fade-out の tail (= event 末尾からの距離)
        // を計算するために必要 (= 旧 sample-domain `event_total_frames` の
        // beat-domain 同等値)。 event 全長を current_bpm で換算するので、
        // tempo 変化で fade duration もスケールする。
        let fade_in_samples =
            (event.fade_in_beats * samples_per_beat).max(0.0) as u64;
        let fade_out_samples =
            (event.fade_out_beats * samples_per_beat).max(0.0) as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let event_total_samples = ((event.end_beat - event.start_beat)
            * samples_per_beat)
            .max(0.0) as u64;
        // event 開始からの absolute sample offset を求めるための起点 (= event
        // start を sample 単位で表現した値、 playhead_beats 基準)。 通常 |.| <
        // 1 buffer worth of samples (= 数千)、 cast overflow 心配なし。
        // 念のため `clamp` で i64 全範囲に収める (= 異常な beat 値で NaN /
        // Inf になる事故を防ぐ defensive)。
        #[allow(clippy::cast_possible_truncation)]
        let event_start_offset_in_buf = ((event.start_beat - playhead_beats)
            * samples_per_beat)
            .clamp(i64::MIN as f64, i64::MAX as f64)
            as i64;
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

            // Phase 5 follow-up (Stretch time-stretch DSP): stretch_mode
            // ごとに source sample 取得経路を分岐する。
            // - Raw / Repitch: 直接 source を読む (linear interp、 Repitch は
            //   effective_pitch_ratio に tempo_ratio 込み)
            // - Stretch: granular synthesis で pitch を保持しつつ tempo に
            //   追随 (= grain hop を tempo_ratio でスケール、 各 grain は
            //   native rate で再生 = pitch 不変)
            // - Slice: transient slicing。 onset (event.onsets) を slice trigger に
            //   slice_sample_at が tempo 追従再生 (= onset 自動検出は r.md #8 B1 で実装)
            let (s_l, s_r) = match event.stretch_mode {
                StretchMode::Stretch => granular_sample_at(
                    event_local,
                    // grain hop が follow_smoothed で伸縮 → source を event 長に
                    // 充填 (= pitch 保持の time-stretch、 ピッチ保持が既定)。
                    // follow_smoothed = stretch_ratio × (smoothed_current_bpm /
                    // nominal_bpm) なので clip 長 stretch + tempo 追従を乗算合成。
                    // Phase 5 follow-up (click 抑制): instant ではなく LP smoothed
                    // bpm 駆動で buffer 境界の Δratio が抑えられ、 grain life
                    // (= ~2*HOP samples) 内の source pos jump が小さくなる (= click
                    // 振幅低減)。 r.md #6: nominal_bpm 基準なので base bpm 変更にも
                    // 追従する (= 旧実装は current/song.bpm 駆動で追従しなかった)。
                    // 完全 click-free には per-event grain-trigger lock-in が必要
                    // (= 別 phase)。
                    follow_smoothed,
                    l_plane,
                    r_plane,
                    event.source_start_frames,
                    event.source_end_frames,
                    buffer.frames,
                    &event.beat_markers,
                    samples_per_beat,
                    event.reversed,
                ),
                StretchMode::Slice => slice_sample_at(
                    event_local,
                    // slice 配置にも clip 長 stretch + tempo 追従を合成 (instant)。
                    follow_instant,
                    l_plane,
                    r_plane,
                    event.source_start_frames,
                    event.source_end_frames,
                    buffer.frames,
                    &event.onsets,
                    event.reversed,
                ),
                _ => {
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
                    #[allow(clippy::cast_sign_loss)]
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
                    (s_l, s_r)
                }
            };

            track_l[i] += s_l * env * pan_l * std::f32::consts::SQRT_2;
            track_r[i] += s_r * env * pan_r * std::f32::consts::SQRT_2;
        }
    }
}

/// Phase 5 follow-up (Stretch time-stretch DSP): granular synthesis で
/// pitch を保持しつつ tempo (= `tempo_ratio`) に追随する。 grain は
/// `GRAIN_LEN_SAMPLES` 幅 + `GRAIN_HOP_SAMPLES` ステップで Hann window を
/// かけて重ね合わせ (50% overlap)、 各 grain は native rate で source を読む
/// (= pitch 不変)。 grain の起点 (= source 内 offset) は出力時刻 × tempo_ratio
/// でスケール → tempo 上昇時に source 進行も速まる (= elastic 流の time
/// stretch、 grain 同士が overlap して時間軸だけ伸縮)。
///
/// MVP scope:
/// - 50% overlap (= hop = len / 2)、 Hann window で 2 grain 和が常時 1.0
/// - linear interpolation 無し (= source 整数 index 直読、 grain 内 pitch
///   不変なので aliasing 影響小)
/// - 渡される `tempo_ratio` (= caller の `follow_smoothed` = stretch_ratio ×
///   smoothed_current_bpm / nominal_bpm) は LP smoothing 済 (= LocalState の
///   `smoothed_current_bpm` を per-buffer に 1-pole LP した値)。 buffer 境界での
///   tempo 変化はここに来た時点で 0.3 coef の LP で抑えられている。 grain life
///   (= ~2*HOP samples ≒ 1 buffer @ 512) 中の Δratio は十分小さく、 通常
///   tempo curve では click が顕著に抑制される
/// - reversed なら source を末尾から読む
///
/// 残課題 (= 別 phase): 完全 click-free は per-event grain-trigger lock-in が
/// 必要。 各 grain k が triggered した瞬間の source position を per-event 状態
/// として保存し、 後続 buffer でもその値を使う。 worker pool に `&mut` 状態を
/// 通す refactor + LocalState に Vec<EventGrainState> 追加 + AudioClipRenderer
/// schedule 変更時の状態 invalidate が必要。 本 commit は LP smoothing で
/// 一般的 tempo curve の click を低減する partial mitigation。
///
/// RT 安全: heap 確保なし、 浮動小数演算のみ。
#[allow(clippy::too_many_arguments)]
fn granular_sample_at(
    event_local: u64,
    tempo_ratio: f64,
    l_plane: &[f32],
    r_plane: &[f32],
    source_start: u64,
    source_end: u64,
    buffer_frames: u64,
    // B12 (r.md #8): warp markers (空 or <2 なら uniform stretch) + その beat 換算用
    // samples_per_beat (= current_bpm 基準、 render loop と同値)。
    beat_markers: &[common::model::BeatMarker],
    samples_per_beat: f64,
    reversed: bool,
) -> (f32, f32) {
    /// grain hop (sample 単位)。 ~12 ms @ 44.1 kHz。 短すぎると metallic、
    /// 長すぎると transient が smear する。 512 で MVP 適正。
    const GRAIN_HOP_SAMPLES: u64 = 512;
    /// grain length (= 2 * hop で 50% overlap)。 Hann window の和が常時 1.0 と
    /// なるための条件。
    const GRAIN_LEN_SAMPLES: u64 = GRAIN_HOP_SAMPLES * 2;

    let source_len = source_end.saturating_sub(source_start);
    if source_len == 0 {
        return (0.0, 0.0);
    }

    // event_local が含まれる grain は最大 2 個 (50% overlap):
    // - k_hi: 直近に開始した grain (= event_local / HOP)
    // - k_lo: 前の grain (= event_local が grain 後半に来ている場合)
    let k_hi = event_local / GRAIN_HOP_SAMPLES;
    let k_lo = k_hi.saturating_sub(1);

    let mut sum_l = 0.0_f32;
    let mut sum_r = 0.0_f32;

    for k in k_lo..=k_hi {
        let grain_start_out = k * GRAIN_HOP_SAMPLES;
        if event_local < grain_start_out {
            continue;
        }
        let in_grain = event_local - grain_start_out;
        if in_grain >= GRAIN_LEN_SAMPLES {
            continue;
        }
        // grain の source 内 offset。 warp markers (≥2) があれば event-local beat
        // (= grain_start_out / samples_per_beat) を warp_source_frame で source
        // frame に写す非一様 stretch、 無ければ uniform (× tempo_ratio)。
        let grain_source_offset = if beat_markers.len() >= 2 && samples_per_beat > 0.0 {
            let event_beat = grain_start_out as f64 / samples_per_beat;
            match common::audio_render::warp_source_frame(event_beat, beat_markers) {
                Some(sf) => (sf - source_start as f64).max(0.0) as u64,
                None => (grain_start_out as f64 * tempo_ratio).max(0.0) as u64,
            }
        } else {
            (grain_start_out as f64 * tempo_ratio).max(0.0) as u64
        };
        let source_pos_in_event = grain_source_offset + in_grain;
        if source_pos_in_event >= source_len {
            continue;
        }
        let abs_idx = if reversed {
            // reversed: source の末尾から読む
            let from_end = source_pos_in_event;
            if from_end >= source_len {
                continue;
            }
            source_start + (source_len - 1 - from_end)
        } else {
            source_start + source_pos_in_event
        };
        if abs_idx >= source_end || abs_idx >= buffer_frames {
            continue;
        }
        // Hann window: 0.5 * (1 - cos(2π * t / (LEN - 1)))。 (LEN-1) は
        // window の最後の点を 0 にするための慣例 (= symmetric Hann)。
        #[allow(clippy::cast_precision_loss)]
        let t = in_grain as f32;
        #[allow(clippy::cast_precision_loss)]
        let len_f = (GRAIN_LEN_SAMPLES - 1) as f32;
        let env_win = 0.5
            * (1.0 - (std::f32::consts::TAU * t / len_f).cos());

        let s_l = l_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
        let s_r = r_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
        sum_l += s_l * env_win;
        sum_r += s_r * env_win;
    }

    (sum_l, sum_r)
}

/// Phase 5 follow-up (StretchMode::Slice): transient-based slice 再生。
/// `onsets` (= source 内 transient sample 位置) で分割した slice を、 各 slice
/// の trigger beat 位置で出力に流す。 slice は **native rate で再生** (= pitch
/// 保持、 source 整数 index 直読)、 slice の trigger 位置は
/// `onsets[i] / tempo_ratio` で出力 sample 位置にマップされる (= 拍子に
/// ロック)。
///
/// MVP scope (Ableton / Live 流の Slice mode):
/// - tempo 上昇 (= ratio > 1): slice が出力上で詰まる → 1 つ前の slice が
///   終了前に次 slice が triggered (= cut)。 出力に「gap が無く詰まる」 感
/// - tempo 下降 (= ratio < 1): slice 間に gap が出る (= silence)。 transient
///   は kept、 slice 末尾の余韻が伸びる
/// - onsets が空: source 全体を 1 slice として再生 (= Raw に近い挙動)
/// - linear interp 無し (= 整数 index 直読、 grain 内 native rate のため
///   aliasing 影響小)
/// - reversed: source 末尾から読む (= slice 内 source position を反転)
///
/// RT 安全: heap 確保なし、 onsets slice の binary search のみ。
#[allow(clippy::too_many_arguments)]
fn slice_sample_at(
    event_local: u64,
    tempo_ratio: f64,
    l_plane: &[f32],
    r_plane: &[f32],
    source_start: u64,
    source_end: u64,
    buffer_frames: u64,
    onsets: &[u64],
    reversed: bool,
) -> (f32, f32) {
    let source_len = source_end.saturating_sub(source_start);
    if source_len == 0 {
        return (0.0, 0.0);
    }

    // onsets が空 / 不足の場合は source 全体を 1 slice (= Raw 等価)。
    if onsets.is_empty() {
        let source_pos = event_local;
        if source_pos >= source_len {
            return (0.0, 0.0);
        }
        let abs_idx = if reversed {
            source_start + (source_len - 1 - source_pos)
        } else {
            source_start + source_pos
        };
        if abs_idx >= source_end || abs_idx >= buffer_frames {
            return (0.0, 0.0);
        }
        let s_l = l_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
        let s_r = r_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
        return (s_l, s_r);
    }

    // 出力 sample 位置 event_local が含まれる slice を探す。 slice i の trigger
    // 出力位置 = `onsets[i] / tempo_ratio` (= nominal で onsets[i] sample 後、
    // tempo 比でスケール)。 binary search で「`onsets[i] / tempo_ratio <=
    // event_local` を満たす最大 i」 を求める。 tempo_ratio が安全な範囲なら
    // `onsets[i] / tempo_ratio` は monotonically increasing。
    if tempo_ratio <= 0.0 {
        return (0.0, 0.0);
    }
    // event_local * tempo_ratio に対応する onsets index を比較で探す
    // (= `onsets[i] <= event_local * tempo_ratio` を満たす最大 i)。
    let threshold = (event_local as f64 * tempo_ratio) as u64;
    // partition_point: onsets[i] <= threshold な要素数 = i のとき返る。 i-1
    // が「該当 slice index」 (= i == 0 なら slice 開始前で silence)。
    let count = onsets.partition_point(|&o| o <= threshold);
    if count == 0 {
        // event_local が onsets[0] / tempo_ratio より前 (= まだ最初の slice 前)
        // の場合は silence。 これは onsets[0] > 0 のときのみ起き、 通常は
        // onsets[0] = 0 で event 開始と同時に最初の slice が triggered。
        return (0.0, 0.0);
    }
    let slice_idx = count - 1;
    let slice_source_start = onsets[slice_idx];
    let slice_source_end = onsets
        .get(slice_idx + 1)
        .copied()
        .unwrap_or(source_len);
    // slice trigger 出力位置 (sample 単位):
    // `onsets[i] / tempo_ratio` の floor。 整数化で 1 sample 単位の誤差。
    let slice_trigger_output = (slice_source_start as f64 / tempo_ratio) as u64;
    if event_local < slice_trigger_output {
        return (0.0, 0.0);
    }
    // slice 内 elapsed (= 出力上の sample 数、 = source 上の sample 数 *
    // 1.0、 native rate なので)
    let slice_local = event_local - slice_trigger_output;
    let source_pos_in_event = slice_source_start + slice_local;
    if source_pos_in_event >= slice_source_end {
        // slice 末尾を越えた (= tempo 下降で gap、 silence で次 slice 待ち)
        return (0.0, 0.0);
    }
    if source_pos_in_event >= source_len {
        return (0.0, 0.0);
    }
    let abs_idx = if reversed {
        source_start + (source_len - 1 - source_pos_in_event)
    } else {
        source_start + source_pos_in_event
    };
    if abs_idx >= source_end || abs_idx >= buffer_frames {
        return (0.0, 0.0);
    }
    let s_l = l_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
    let s_r = r_plane.get(abs_idx as usize).copied().unwrap_or(0.0);
    (s_l, s_r)
}
