//! Audio clip renderer.
//!
//! Defines the data structures the live audio thread reads every buffer
//! to mix audio events into per-track scratch buffers. PR2 stood up
//! the types + an empty default so `EngineShared::audio_clip_renderer`
//! has a wait-free snapshot to `load()` from day one; PR6 added the
//! schedule compiler + Raw / Repitch render loop on top.
//!
//! VOICEVOX vocal output is NOT rendered here: vocal clips are MIDI-shaped
//! (lyrics + notes) and play through the builtin VOICEVOX instrument plugin
//! inside daw_plugin_host (PR-V4 — 旧 `SetGeneratedAudio` wire 経路と
//! engine 側 vocal block は撤去済み)。 this module only handles
//! `AudioContent` (imported / bounced WAV) events.
//!
//! Spec: `docs/plan_audio_clip.md` §6 / §9.3.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use common::audio_render::{
    fade_envelope, pitch_factor, sample_rate_ratio, stretch_ratio_for, tempo_follow_ratio,
};
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
    /// **時間軸**の SR 換算比 (= `source_sr / engine_sr`)。 出力 sample を source
    /// frame へ写す全経路 (Raw/Repitch の stride、 granular の grain 配置、 slice の
    /// trigger 写像) に掛かる。 pitch とは独立。
    pub sr_ratio: f64,
    /// **ピッチ軸**の比 (= `2^(semitones/12)`)。 tape 系 (Raw / Repitch) は source を
    /// 読む速度そのものに掛かる (長さも変わる)。 granular (Stretch) / Slice は grain /
    /// slice の **内部読み出し**にだけ掛かり **配置**には掛からない (= 長さを変えずに
    /// 移調)。
    pub pitch_factor: f64,
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

/// Decode an audio file into a planar `AudioSourceBuffer`. Delegates format
/// handling to `common::audio_decode` (symphonia), so daw_audio plays back
/// every format the GUI can import — WAV / AIFF / FLAC / MP3 / OGG / M4A
/// (r.md #19). File-backed sources are decoded **independently per process**
/// (no bulk PCM crosses the IPC wire — arch invariant #2 / §6.1 / §8.3); this
/// is the audio engine's copy of that decode.
pub fn decode_audio(path: &Path) -> Result<AudioSourceBuffer> {
    let decoded = common::audio_decode::decode_audio_file(path)
        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))?;
    Ok(AudioSourceBuffer {
        sample_rate: decoded.sample_rate,
        channels: decoded.channels,
        frames: decoded.frames,
        samples: decoded.samples,
    })
}

/// Build an `AudioClipRenderer` snapshot from the current Song. Walks
/// `Song.audio_sources` (decoding file-backed entries via `common::audio_decode`), then
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
    for (&id, source) in &song.media.audio_sources {
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
                match decode_audio(&abs) {
                    Ok(buf) => Arc::new(buf),
                    Err(e) => {
                        tracing::error!(error = ?e, path = %abs.display(), "decode failed");
                        continue;
                    }
                }
            }
            AudioSourcePath::Absolute(abs) => match decode_audio(abs) {
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
                // 時間軸 (SR 比) と ピッチ軸 (semitone) は直交した 2 量として持ち、
                // どちらをどこに掛けるかは render loop が mode ごとに決める
                // (旧 `pitch_ratio_for` は mode 分岐でピッチ比を捨てており、
                // Raw / Stretch / Slice で inspector のピッチが無反応だった)。
                let sr_ratio = sample_rate_ratio(buffer.sample_rate, engine_sample_rate);
                let pitch_factor = pitch_factor(event.pitch_semitones);
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
                    sr_ratio,
                    pitch_factor,
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
    song.media.audio_sources.iter().any(|(id, source)| {
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
/// E5 sibling (r.md #8): Repitch (tape) mode の連続 source 位置を 1 sample ぶん進める。
/// `state = (last_event_local, accumulated_source_pos)`。 contiguous 再生 (`event_local ==
/// last + 1`) では `ratio` を積分 (= 位置が連続) し、 tempo automation で ratio が変わっても
/// 絶対位置が跳ばない。 不連続 (seek / schedule 変化 / 初回 `last == u64::MAX`) では現 ratio で
/// `event_local × ratio` に再 anchor する。 Raw mode は ratio 一定なので積分値は
/// `event_local × ratio` に一致し従来挙動と byte 同一。
fn repitch_source_pos(state: &mut (u64, f64), event_local: u64, ratio: f64) -> f64 {
    if state.0 != u64::MAX && state.0.wrapping_add(1) == event_local {
        state.1 += ratio;
    } else {
        state.1 = event_local as f64 * ratio;
    }
    state.0 = event_local;
    state.1
}

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
    // E5 (r.md #8): per-track の granular grain-trigger lock-in ring 群 (event 単位)。
    // track 内 event を schedule 順に enumerate した index で引く。 Stretch mode の
    // granular_sample_at に &mut で渡し、 grain offset を trigger 時に固定して tempo 変化での
    // click を防ぐ。 容量を超える index の event は lock 無し (= 従来挙動) に degrade。
    granular_rings: &mut [GrainLockRing],
    // E5 sibling (r.md #8): Repitch (tape) mode の連続 source 位置 accumulator (event 単位、
    // granular_rings と同じ index)。 `(last_event_local, accumulated_source)`。 tempo 変化で
    // 位置が跳ぶ click を防ぐ (granular の lock-in と同 root cause)。
    repitch_accum: &mut [(u64, f64)],
) {
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let n = frames as usize;
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    let buf_end_beats =
        playhead_beats + f64::from(frames) / samples_per_beat;

    // E5 (r.md #8): track 内 event を schedule 順に数える安定 index (lock-in ring の添字)。
    // track_idx filter 後・overlap skip 前に増やすので、 同じ event は buffer を跨いで同じ
    // index になる (seek / schedule 変化は ring 側の grain-k 不一致で自己無効化)。
    let mut track_event_seq = 0usize;
    for event in &renderer.schedule {
        if event.track_idx != track_idx {
            continue;
        }
        let ring_idx = track_event_seq;
        track_event_seq += 1;
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
        // 「出力 sample → source frame」 の **時間軸** 換算 (= SR 比)。 退化値
        // (0 / 負 / NaN) は 1.0 に倒す defensive (source が止まって drone 化しない)。
        // ※ この下の mode 別合成 (time_stride / read_stride) は、 波形描画側の
        // `common::audio_render::event_wave_spans` と同じ写像でなければならない
        // (= 描いた波形と鳴る音が一致する条件)。 片方だけ変えると
        // 下の `wave_span_binding_tests` (実レンダリング出力と span 列の突き合わせ)
        // が落ちる。
        let time_stride = if event.sr_ratio > 0.0 { event.sr_ratio } else { 1.0 };
        // source を **読む** 速度 = 時間軸 × ピッチ軸。 granular / slice はこれを
        // grain / slice の内部読み出しにだけ使い、 配置には `time_stride` を使う
        // ので、 移調しても長さが変わらない (= pitch 保持ストレッチ + 独立移調)。
        let read_stride = if event.pitch_factor > 0.0 {
            time_stride * event.pitch_factor
        } else {
            time_stride
        };
        let effective_pitch_ratio = match event.stretch_mode {
            // Repitch (tape 式) は clip 長 stretch + tempo 追従が再生速度に乗る
            // (= pitch も一緒に変わる、 vinyl 流)。 Raw は stretch_ratio / tempo を
            // 無視 (= 時間操作しない定義、 native rate で trim/cut) が、 ピッチ指定は
            // tape として効く (= Ableton Warp-off + Transpose 相当)。
            StretchMode::Repitch => read_stride * follow_instant,
            _ => read_stride,
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

        // E5 (r.md #8): この event の granular lock-in ring (index 超過は None = lock 無しに
        // degrade)。 sample loop の granular_sample_at へ毎サンプル reborrow で渡す。
        let mut lock_ring = granular_rings.get_mut(ring_idx);
        // E5 sibling: Repitch の連続 source 位置 accumulator (同 event index)。
        let mut repitch_state = repitch_accum.get_mut(ring_idx);

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
                    time_stride,
                    read_stride,
                    l_plane,
                    r_plane,
                    event.source_start_frames,
                    event.source_end_frames,
                    buffer.frames,
                    &event.beat_markers,
                    samples_per_beat,
                    event.reversed,
                    lock_ring.as_deref_mut(),
                ),
                StretchMode::Slice => slice_sample_at(
                    event_local,
                    // slice 配置にも clip 長 stretch + tempo 追従を合成 (instant)。
                    follow_instant,
                    time_stride,
                    read_stride,
                    l_plane,
                    r_plane,
                    event.source_start_frames,
                    event.source_end_frames,
                    buffer.frames,
                    &event.onsets,
                    event.reversed,
                ),
                _ => {
                    // Source position with linear interpolation. effective_pitch_ratio は
                    // Repitch mode で tempo ratio スケール済 (tempo automation で変動)、 Raw は
                    // 不変。 E5 sibling (r.md #8): 連続 accumulator で源位置を積分し、 tempo 変化で
                    // `event_local × ratio` の絶対値が跳ぶ click を防ぐ (jump 量は event_local に
                    // 比例 = granular より重症)。 contiguous 再生では ratio を積分、 seek/schedule
                    // 変化 (event_local 不連続) では現 ratio で再 anchor。 Raw は ratio 一定なので
                    // 積分値 = `event_local × ratio` で従来と一致 (無害)。 容量超過 event は None で degrade。
                    let source_pos = match repitch_state.as_deref_mut() {
                        Some(state) => {
                            repitch_source_pos(state, event_local, effective_pitch_ratio)
                        }
                        None => event_local as f64 * effective_pitch_ratio,
                    };
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
/// E5 (r.md #8): per-event grain-trigger lock-in ring。 uniform-stretch の grain `k` の
/// source offset を **trigger 時の値に固定**するため、 `(k, offset)` を `k % len` slot に
/// 記録する。 後続 buffer で tempo_ratio が変わっても同じ grain は記録済 offset を再利用し、
/// source position の跳び (= click) を防ぐ。 slot の `k` 不一致 (seek / schedule 変化で grain
/// 列がずれた) は自動で recompute される。 50% overlap で同時 active grain は 2 個なので 8 slot
/// あれば retire まで上書きされない。
pub type GrainLockRing = [(u64, u64); 8];

/// event-local な **小数** source 位置 `pos_in_event` (= `source_start` 起点の
/// source frame) から linear interpolation で 1 frame 取り出す。 granular /
/// slice が source SR ≠ engine SR で小数進度になるため、 整数 index 直読では
/// 1 frame 単位の量子化ノイズが乗る (44.1k→48k は毎 sample 位相がずれる)。
/// 範囲外 (負 / NaN / source 窓外 / buffer 外) は `None`。
/// RT 安全: 確保なし・panic なし (`slice::get` で境界を吸収)。
#[inline]
#[allow(clippy::too_many_arguments)]
fn source_frame_lerp(
    l_plane: &[f32],
    r_plane: &[f32],
    source_start: u64,
    source_end: u64,
    buffer_frames: u64,
    source_len: u64,
    pos_in_event: f64,
    reversed: bool,
) -> Option<(f32, f32)> {
    // NaN / Inf (退化した ratio の伝播) も含めて弾く。
    if !pos_in_event.is_finite() || pos_in_event < 0.0 || pos_in_event >= source_len as f64 {
        return None;
    }
    // reversed は source 窓の末尾から手前へ読む (小数位置のまま反転)。
    let abs = source_start as f64
        + if reversed {
            (source_len - 1) as f64 - pos_in_event
        } else {
            pos_in_event
        };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let i0 = abs.floor() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let frac = (abs - i0 as f64) as f32;
    if i0 >= source_end || i0 >= buffer_frames {
        return None;
    }
    let l0 = l_plane.get(i0 as usize).copied().unwrap_or(0.0);
    let r0 = r_plane.get(i0 as usize).copied().unwrap_or(0.0);
    // 次 frame が窓外なら補間せず l0/r0 を保持 (= 端で 0 に落ちない)。
    let i1 = i0 + 1;
    let (l1, r1) = if i1 < source_end && i1 < buffer_frames {
        (
            l_plane.get(i1 as usize).copied().unwrap_or(l0),
            r_plane.get(i1 as usize).copied().unwrap_or(r0),
        )
    } else {
        (l0, r0)
    };
    Some((l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac))
}

#[allow(clippy::too_many_arguments)]
fn granular_sample_at(
    event_local: u64,
    tempo_ratio: f64,
    // **時間軸** の換算比 (= `source_sr / engine_sr`)。 grain の **配置**
    // (`grain_start_out × tempo_ratio × time_stride`) に掛かる。 これを落とすと
    // 44.1 kHz 素材を 48 kHz エンジンで再生したとき source を 8.8% 速く消費し、
    // clip 末尾が無音になる (= 「波形より音が短い」)。
    time_stride: f64,
    // grain **内部** の読み進み (= `time_stride × pitch_factor`)。 配置と分けて
    // あるので、 移調しても grain の配置 = 出力長は変わらない (granular pitch
    // shift: 長さは配置が、 音程は読み速度が決める)。
    read_stride: f64,
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
    // E5 (r.md #8): uniform-stretch grain offset の lock-in ring。 Some なら grain k の
    // source offset を trigger 時の値に固定 (tempo 変化での source position 跳び = click を
    // 防ぐ)。 warp path (markers) は beat の決定論関数なので不使用。 None で従来挙動。
    mut lock_ring: Option<&mut GrainLockRing>,
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
        // frame に写す非一様 stretch、 無ければ uniform (× tempo_ratio × time_stride)。
        // warp path の戻り値は既に source frame 空間なので time_stride は掛けない
        // (掛けるのは「出力 sample → source frame」 の換算のみ)。
        let grain_source_offset = if beat_markers.len() >= 2 && samples_per_beat > 0.0 {
            let event_beat = grain_start_out as f64 / samples_per_beat;
            match common::audio_render::warp_source_frame(event_beat, beat_markers) {
                Some(sf) => (sf - source_start as f64).max(0.0) as u64,
                None => (grain_start_out as f64 * tempo_ratio * time_stride).max(0.0) as u64,
            }
        } else {
            // E5: uniform stretch。 lock_ring があれば grain k の source offset を **trigger 時の
            // 値に固定** する。 tempo_ratio は buffer ごとに変わりうる (tempo automation) ので、
            // 同じ grain を後続 buffer で recompute すると source position が跳んで click になる。
            let recomputed = (grain_start_out as f64 * tempo_ratio * time_stride).max(0.0) as u64;
            if let Some(ring) = lock_ring.as_deref_mut() {
                let slot = (k as usize) % ring.len();
                if ring[slot].0 == k {
                    ring[slot].1
                } else {
                    ring[slot] = (k, recomputed);
                    recomputed
                }
            } else {
                recomputed
            }
        };
        // grain 内は `read_stride` (= SR 比 × ピッチ比) で読み進む。 配置とは独立
        // なので、 移調しても出力長は変わらない。 小数位置は補間する。
        let source_pos_in_event = grain_source_offset as f64 + in_grain as f64 * read_stride;
        let Some((s_l, s_r)) = source_frame_lerp(
            l_plane,
            r_plane,
            source_start,
            source_end,
            buffer_frames,
            source_len,
            source_pos_in_event,
            reversed,
        ) else {
            continue;
        };
        // Hann window: 0.5 * (1 - cos(2π * t / (LEN - 1)))。 (LEN-1) は
        // window の最後の点を 0 にするための慣例 (= symmetric Hann)。
        #[allow(clippy::cast_precision_loss)]
        let t = in_grain as f32;
        #[allow(clippy::cast_precision_loss)]
        let len_f = (GRAIN_LEN_SAMPLES - 1) as f32;
        let env_win = 0.5
            * (1.0 - (std::f32::consts::TAU * t / len_f).cos());

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
    // **時間軸** の換算比 (= `source_sr / engine_sr`)。 slice の **trigger 位置の
    // 写像** に効く。 落とすと source SR ≠ engine SR で slice 境界が出力上でずれる。
    time_stride: f64,
    // slice **内部** の読み進み (= `time_stride × pitch_factor`)。 移調すると slice
    // 本体が速く / 遅くなる (= Ableton Beats mode の Transpose と同じで、 trigger
    // グリッドは動かず slice の鳴る長さだけ変わる)。
    read_stride: f64,
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

    // onsets が空 / 不足の場合は source 全体を 1 slice (= event 頭で 1 回 trigger、
    // 以降 native rate 再生)。 slice の定義どおり時間伸縮はしない (伸ばした分の
    // 余りは無音、 縮めた分は cut)。
    if onsets.is_empty() {
        return source_frame_lerp(
            l_plane,
            r_plane,
            source_start,
            source_end,
            buffer_frames,
            source_len,
            event_local as f64 * read_stride,
            reversed,
        )
        .unwrap_or((0.0, 0.0));
    }

    // 出力 sample 位置 event_local が含まれる slice を探す。 出力 sample →
    // source frame の写像率 `map_rate` (= 時間軸の伸縮 × SR 比) で両空間を往復する:
    // slice i の trigger 出力位置 = `onsets[i] / map_rate`。 binary search で
    // 「`onsets[i] / map_rate <= event_local` を満たす最大 i」 を求める
    // (map_rate > 0 なら monotonically increasing)。
    let map_rate = tempo_ratio * time_stride;
    if !map_rate.is_finite() || map_rate <= 0.0 {
        return (0.0, 0.0);
    }
    // event_local * map_rate に対応する onsets index を比較で探す
    // (= `onsets[i] <= event_local * map_rate` を満たす最大 i)。
    let threshold = (event_local as f64 * map_rate) as u64;
    // partition_point: onsets[i] <= threshold な要素数 = i のとき返る。 i-1
    // が「該当 slice index」 (= i == 0 なら slice 開始前で silence)。
    let count = onsets.partition_point(|&o| o <= threshold);
    if count == 0 {
        // event_local が onsets[0] / map_rate より前 (= まだ最初の slice 前)
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
    // slice trigger 出力位置 (sample 単位): `onsets[i] / map_rate` の floor。
    // 整数化で 1 sample 単位の誤差。
    let slice_trigger_output = (slice_source_start as f64 / map_rate) as u64;
    if event_local < slice_trigger_output {
        return (0.0, 0.0);
    }
    // slice 内 elapsed。 出力上の sample 数に read_stride を掛けて source frame
    // へ写す (= slice 本体は native rate 再生、 移調時のみその比で速くなる)。
    let slice_local = event_local - slice_trigger_output;
    let source_pos_in_event = slice_source_start as f64 + slice_local as f64 * read_stride;
    if source_pos_in_event >= slice_source_end as f64 {
        // slice 末尾を越えた (= tempo 下降で gap、 silence で次 slice 待ち)
        return (0.0, 0.0);
    }
    source_frame_lerp(
        l_plane,
        r_plane,
        source_start,
        source_end,
        buffer_frames,
        source_len,
        source_pos_in_event,
        reversed,
    )
    .unwrap_or((0.0, 0.0))
}

/// Stretch / Slice の「出力 sample → source frame」 写像が source SR ≠ engine SR でも
/// 正しいことを検証する (= source SR 決め打ちの回帰防止)。 素材に ramp (0→1) を使い、
/// 「出力の f 地点で source の f 地点が鳴っている」 を直接 assert する。
/// 修正前は 44.1 kHz 素材 / 48 kHz engine で source を 8.8% 速く消費し、 clip 末尾
/// 8.1% が無音になっていた (= 「波形より音が短い」)。
#[cfg(test)]
mod stretch_sample_rate_tests {
    use super::*;

    const ENGINE_SR: u32 = 48_000;
    const BPM: f32 = 120.0;
    /// 4 拍 @ 120 BPM = 2 秒 (= 1 秒素材の 2 倍に stretch)。
    const LEN_BEATS: f64 = 4.0;

    /// 1 秒ぶんの ramp (0.0 → 1.0) 素材。 出力値がそのまま「source のどこを
    /// 読んでいるか」 を表すので、 時間写像を直接 assert できる。
    fn ramp_source(source_sr: u32) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames).map(|i| i as f32 / (frames - 1) as f32).collect();
        AudioSourceBuffer {
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    /// clip 全長 (2 秒) を 512 frame ずつ render して L channel を連結する。
    fn render_clip(source_sr: u32, mode: StretchMode, onsets: Vec<u64>) -> Vec<f32> {
        render_clip_pitched(source_sr, mode, onsets, 0.0)
    }

    /// `semitones` 付きの render (= inspector のピッチ指定に相当)。
    fn render_clip_pitched(
        source_sr: u32,
        mode: StretchMode,
        onsets: Vec<u64>,
        semitones: f32,
    ) -> Vec<f32> {
        let buffer = ramp_source(source_sr);
        let source_frames = buffer.frames;
        let event = RenderedEvent {
            track_idx: 0,
            clip_idx: 0,
            start_beat: 0.0,
            end_beat: LEN_BEATS,
            source_id: 1,
            source_start_frames: 0,
            source_end_frames: source_frames,
            gain_lin: 1.0,
            pan: 0.0,
            sr_ratio: sample_rate_ratio(source_sr, ENGINE_SR),
            pitch_factor: pitch_factor(semitones),
            stretch_ratio: stretch_ratio_for(source_frames, source_sr, LEN_BEATS, BPM),
            nominal_bpm: BPM,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: false,
            stretch_mode: mode,
            onsets,
            beat_markers: Vec::new(),
        };
        let mut sources = HashMap::new();
        sources.insert(1u32, Arc::new(buffer));
        let renderer = AudioClipRenderer { schedule: vec![event], sources };

        let samples_per_beat = f64::from(ENGINE_SR) * 60.0 / f64::from(BPM);
        let total = (LEN_BEATS * samples_per_beat) as usize;
        let mut rings = vec![[(u64::MAX, 0u64); 8]; 4];
        let mut accum = vec![(u64::MAX, 0.0f64); 4];
        let mut out = Vec::with_capacity(total);
        while out.len() < total {
            let frames = 512.min(total - out.len());
            let mut l = vec![0.0f32; frames];
            let mut r = vec![0.0f32; frames];
            render_audio_events(
                &renderer,
                0,
                &mut l,
                &mut r,
                out.len() as f64 / samples_per_beat,
                BPM,
                ENGINE_SR,
                frames as u32,
                f64::from(BPM),
                &mut rings,
                &mut accum,
            );
            out.extend_from_slice(&l);
        }
        // pan 中央 (equal-power × √2) は利得 1.0 に戻る前提を固定する。
        out
    }

    /// 出力の位置 `f` (0..1) で source の位置 `f` が鳴っている = 時間写像が正しい。
    /// 44.1 kHz 素材を 48 kHz engine で鳴らしても clip の端まで音が続く。
    #[test]
    fn stretch_maps_output_time_to_source_time_at_any_sample_rate() {
        for source_sr in [48_000u32, 44_100, 96_000] {
            let out = render_clip(source_sr, StretchMode::Stretch, Vec::new());
            let total = out.len() as f64;
            for f in [0.25_f64, 0.5, 0.75, 0.95] {
                let idx = (total * f) as usize;
                let got = out[idx];
                assert!(
                    (f64::from(got) - f).abs() < 0.03,
                    "source {source_sr} Hz: 出力 {f} 地点で source {f} 地点が鳴るべき、 got {got}"
                );
            }
            // 末尾がまるごと無音になっていないこと (= 「波形より音が短い」 の回帰検出)。
            let tail = out[(total * 0.99) as usize];
            assert!(
                tail > 0.85,
                "source {source_sr} Hz: clip 末尾まで鳴るべき、 got {tail}"
            );
        }
    }

    /// 最後に音が出ている出力 frame (= source を使い切った位置)。
    fn last_audible(out: &[f32]) -> usize {
        out.iter()
            .rposition(|s| s.abs() > 1e-4)
            .unwrap_or(0)
    }

    /// granular の **ピッチ** は grain 内部の読み速度だけを変え、 grain の **配置**
    /// (= 出力長) は変えない。 grain 0 のみが active な区間 (in_grain < HOP) で
    /// 同じ window 係数が掛かるので、 出力比 = read_stride 比になる。
    #[test]
    fn pitch_scales_in_grain_read_rate_only() {
        // ramp plane: 値 = index / (len-1) なので、 読み位置がそのまま値に出る。
        let len = 48_000usize;
        let plane: Vec<f32> = (0..len).map(|i| i as f32 / (len - 1) as f32).collect();
        let at = |read_stride: f64| {
            granular_sample_at(
                300, // grain 0 のみ active (< GRAIN_HOP_SAMPLES)
                1.0,
                1.0,
                read_stride,
                &plane,
                &plane,
                0,
                len as u64,
                len as u64,
                &[],
                512.0,
                false,
                None,
            )
            .0
        };
        let base = at(1.0);
        let octave_up = at(2.0);
        assert!(base > 0.0, "基準が無音では比が取れない: {base}");
        assert!(
            (f64::from(octave_up / base) - 2.0).abs() < 1e-3,
            "+1 oct で grain 内の読み速度が 2 倍になるべき、 got {}",
            octave_up / base
        );
    }

    /// Stretch (granular) で移調しても **長さは変わらない** (= pitch 保持ストレッチ
    /// と独立した移調)。 修正前は semitone が捨てられ、 そもそも音程が動かなかった。
    #[test]
    fn pitch_shift_keeps_length_in_stretch_mode() {
        let plain = render_clip_pitched(48_000, StretchMode::Stretch, Vec::new(), 0.0);
        let up = render_clip_pitched(48_000, StretchMode::Stretch, Vec::new(), 12.0);
        let down = render_clip_pitched(48_000, StretchMode::Stretch, Vec::new(), -12.0);
        for (label, out) in [("+12", &up), ("-12", &down)] {
            let ratio = last_audible(out) as f64 / last_audible(&plain) as f64;
            assert!(
                (ratio - 1.0).abs() < 0.02,
                "{label} 半音でも clip 長は不変であるべき、 got {ratio}"
            );
        }
    }

    /// tape 系 (Raw / Repitch) と Slice は、 移調がそのまま再生速度になる
    /// (= +1 oct で source を 2 倍速で消費 → 鳴る長さが半分)。 修正前は Raw /
    /// Slice が semitone を無視していた (inspector のピッチが無反応)。
    #[test]
    fn pitch_scales_playback_rate_in_tape_and_slice_modes() {
        for mode in [StretchMode::Raw, StretchMode::Repitch, StretchMode::Slice] {
            let plain = render_clip_pitched(48_000, mode, Vec::new(), 0.0);
            let up = render_clip_pitched(48_000, mode, Vec::new(), 12.0);
            let ratio = last_audible(&up) as f64 / last_audible(&plain) as f64;
            assert!(
                (ratio - 0.5).abs() < 0.02,
                "{mode:?}: +1 oct で 2 倍速 (= 鳴る長さ半分) になるべき、 got {ratio}"
            );
        }
    }

    /// Slice は「slice の trigger 位置だけが伸縮し、 slice 本体は native rate」。
    /// trigger の写像にも SR 比が要る (落とすと slice 境界が出力上でずれる)。
    #[test]
    fn slice_triggers_map_with_sample_rate_ratio() {
        let source_sr = 44_100u32;
        // 素材の中央 (0.5 秒) に 2 つ目の slice。
        let onsets = vec![0u64, 22_050];
        let out = render_clip(source_sr, StretchMode::Slice, onsets);

        // slice 0 は native rate なので出力 0.5 秒 (= 24000 frame) で素材の中央に達し、
        // slice 末尾を越えて gap になる。
        assert!(out[23_000] > 0.4, "slice 0 の末尾直前は鳴っている: {}", out[23_000]);
        assert!(
            out[30_000].abs() < 1e-6,
            "slice 0 終了後・slice 1 trigger 前は gap: {}",
            out[30_000]
        );
        // slice 1 の trigger 出力位置 = 22050 / (stretch 0.5 × sr 0.91875) = 48000 frame。
        assert!(
            out[47_900].abs() < 1e-6,
            "trigger 直前はまだ gap: {}",
            out[47_900]
        );
        let after = out[49_000];
        let expected = (22_050.0 + 1_000.0 * 44_100.0 / 48_000.0) / 44_099.0;
        assert!(
            (f64::from(after) - expected).abs() < 0.03,
            "slice 1 は素材中央から native rate で再生されるべき、 expected {expected}, got {after}"
        );
    }
}

/// r.md #41: **波形描画の写像 (`common::audio_render::event_wave_spans`) と、
/// 実レンダリング出力が一致する**ことを直接 assert する束縛テスト。
///
/// 従来この 2 つはコメント (`render_audio_events` の time_stride / read_stride 節)
/// でしか結び付いておらず、 片方だけ変えても CI が落ちなかった。 ここで
/// 「span が張られている拍区間は鳴っていて、 span の source 写像どおりの音が出る」
/// 「span が無い拍区間は完全に無音」 を ramp 素材で検証し、 描画と再生の乖離を
/// 構造的に検出できるようにする。
#[cfg(test)]
mod wave_span_binding_tests {
    use super::*;
    use common::audio_render::{WaveSpan, event_wave_spans};
    use common::model::AudioEvent;

    const ENGINE_SR: u32 = 48_000;
    const BPM: f32 = 120.0;
    /// 1 秒 (= 120 BPM で 2 拍) ぶんの ramp 素材。
    const SOURCE_FRAMES: u64 = 48_000;

    /// 値 = 位置 の ramp 素材 (出力値がそのまま「source のどこを読んでいるか」)。
    fn ramp(source_sr: u32) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames)
            .map(|i| i as f32 / (frames - 1) as f32)
            .collect();
        AudioSourceBuffer {
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    fn slice_event(len_beats: f64, semis: f32, onsets: Vec<u64>) -> AudioEvent {
        AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: len_beats,
            stretch_mode: StretchMode::Slice,
            pitch_semitones: semis,
            onsets,
            ..AudioEvent::default()
        }
    }

    /// model の `AudioEvent` を `compile_audio_schedule` と **同じ写像** で
    /// `RenderedEvent` に落とし、 clip 全長を 512 frame ずつ render する。
    ///
    /// `tempo_song` を渡すと engine (`daw_audio::engine`) と同じく **buffer ごとに**
    /// `evaluate_song_tempo(song, playhead_beats)` で current_bpm を評価し、
    /// `playhead_beats += frames * bpm / (60*SR)` で進める (= tempo automation 下の
    /// 実挙動を再現)。 `None` なら定数 `BPM`。
    fn render_model_event_with_tempo(
        event: &AudioEvent,
        source_sr: u32,
        tempo_song: Option<&common::model::Song>,
    ) -> Rendered {
        let buffer = ramp(source_sr);
        let mut onsets = event.onsets.clone();
        onsets.sort_unstable();
        onsets.dedup();
        let mut beat_markers = event.beat_markers.clone();
        beat_markers.sort_by(|a, b| a.locked_beat.total_cmp(&b.locked_beat));
        beat_markers.dedup_by(|a, b| (a.locked_beat - b.locked_beat).abs() < 1e-9);
        let rendered = RenderedEvent {
            track_idx: 0,
            clip_idx: 0,
            start_beat: 0.0,
            end_beat: event.event_length_beats,
            source_id: 1,
            source_start_frames: event.source_start_frames,
            source_end_frames: event.source_end_frames,
            gain_lin: 1.0,
            pan: 0.0,
            sr_ratio: sample_rate_ratio(buffer.sample_rate, ENGINE_SR),
            pitch_factor: pitch_factor(event.pitch_semitones),
            stretch_ratio: stretch_ratio_for(
                event.source_end_frames.saturating_sub(event.source_start_frames),
                buffer.sample_rate,
                event.event_length_beats,
                BPM,
            ),
            nominal_bpm: BPM,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: event.reversed,
            stretch_mode: event.stretch_mode,
            onsets,
            beat_markers,
        };
        let mut sources = HashMap::new();
        sources.insert(1u32, Arc::new(buffer));
        let renderer = AudioClipRenderer { schedule: vec![rendered], sources };

        let mut rings = vec![[(u64::MAX, 0u64); 8]; 4];
        let mut accum = vec![(u64::MAX, 0.0f64); 4];
        // clip 全長を鳴らし切るまで (tempo 曲線下では拍あたりの sample 数が変わる)。
        let mut r = Rendered { out: Vec::new(), marks: Vec::new() };
        let mut playhead_beats = 0.0_f64;
        while playhead_beats < event.event_length_beats {
            // engine と同じ: buffer 先頭の playhead で tempo を評価し、 その buffer 内は定数。
            let current_bpm = match tempo_song {
                Some(s) => common::automation::evaluate_song_tempo(s, playhead_beats),
                None => BPM,
            };
            let frames = 512usize;
            let mut l = vec![0.0f32; frames];
            let mut rr = vec![0.0f32; frames];
            r.marks.push((playhead_beats, r.out.len()));
            render_audio_events(
                &renderer,
                0,
                &mut l,
                &mut rr,
                playhead_beats,
                current_bpm,
                ENGINE_SR,
                frames as u32,
                f64::from(current_bpm),
                &mut rings,
                &mut accum,
            );
            r.out.extend_from_slice(&l);
            playhead_beats += frames as f64 * f64::from(current_bpm) / (60.0 * f64::from(ENGINE_SR));
        }
        r.marks.push((playhead_beats, r.out.len()));
        r
    }

    /// render 結果 + buffer 境界ごとの (playhead 拍, 出力 sample index)。
    /// tempo 曲線下では拍 → sample が非線形なので、 この対応表で引く。
    struct Rendered {
        out: Vec<f32>,
        marks: Vec<(f64, usize)>,
    }

    impl Rendered {
        /// 拍 `beat` に対応する出力 sample index (buffer 境界間は線形補間)。
        fn index_at_beat(&self, beat: f64) -> usize {
            let i = self
                .marks
                .partition_point(|&(b, _)| b <= beat)
                .saturating_sub(1);
            let (b0, s0) = self.marks[i];
            let Some(&(b1, s1)) = self.marks.get(i + 1) else {
                return s0;
            };
            if b1 <= b0 {
                return s0;
            }
            let f = ((beat - b0) / (b1 - b0)).clamp(0.0, 1.0);
            s0 + ((s1 - s0) as f64 * f) as usize
        }
    }

    fn render_model_event(event: &AudioEvent, source_sr: u32) -> Rendered {
        render_model_event_with_tempo(event, source_sr, None)
    }

    /// span の x 方向 fraction `f` が指す source frame (widget の写像と同じ:
    /// `reversed` なら左端が `source_end`)。
    fn span_source_at(s: &WaveSpan, f: f64) -> f64 {
        let len = (s.source_end - s.source_start) as f64;
        if s.reversed {
            s.source_end as f64 - len * f
        } else {
            s.source_start as f64 + len * f
        }
    }

    /// 「span が張られた区間は span の写像どおりに鳴り、 span 外は完全無音」 を assert。
    ///
    /// `tol` は ramp 値 (= source 位置を 0..1 に正規化した値) の許容差。 tape / slice は
    /// sample 直読なので 0.02、 granular は grain hop 512 で source offset が量子化される
    /// ぶん緩める。
    fn assert_render_matches_spans_with(
        event: &AudioEvent,
        source_sr: u32,
        tempo_song: Option<&common::model::Song>,
        tol: f32,
        label: &str,
    ) {
        let r = render_model_event_with_tempo(event, source_sr, tempo_song);
        let mut spans = Vec::new();
        let tempo = match tempo_song {
            Some(s) => common::audio_render::TempoMap::from_song(s),
            None => common::audio_render::TempoMap::constant(BPM),
        };
        event_wave_spans(event, source_sr, &tempo, 0.0, &mut spans);
        assert!(!spans.is_empty(), "{label}: span が 1 本も無い");
        let src_max = (u64::from(source_sr) - 1) as f64;
        // 境界の整数丸め (trigger の floor / buffer 分割) を避ける inset。
        const INSET: usize = 64;

        for (i, s) in spans.iter().enumerate() {
            let dur = s.end_beat - s.start_beat;
            for f in [0.25_f64, 0.5, 0.75] {
                let idx = r.index_at_beat(s.start_beat + dur * f);
                if idx >= r.out.len() {
                    continue;
                }
                let expect = (span_source_at(s, f) / src_max) as f32;
                let got = r.out[idx];
                assert!(
                    (got - expect).abs() < tol,
                    "{label}: span {i} ({s:?}) の {f} 地点は source {expect} が鳴るべき、 got {got}"
                );
            }
        }

        // span と span の隙間 (= gap) / 先頭前 / 末尾後は engine も完全無音。
        let mut silent_ranges: Vec<(f64, f64)> = Vec::new();
        if spans[0].start_beat > 0.0 {
            silent_ranges.push((0.0, spans[0].start_beat));
        }
        for w in spans.windows(2) {
            if w[1].start_beat > w[0].end_beat {
                silent_ranges.push((w[0].end_beat, w[1].start_beat));
            }
        }
        let last_end = spans[spans.len() - 1].end_beat;
        if last_end < event.event_length_beats {
            silent_ranges.push((last_end, event.event_length_beats));
        }
        for (b0, b1) in silent_ranges {
            let lo = r.index_at_beat(b0) + INSET;
            let hi = r.index_at_beat(b1).min(r.out.len()).saturating_sub(INSET);
            if hi <= lo {
                continue;
            }
            for (k, v) in r.out[lo..hi].iter().enumerate() {
                assert!(
                    v.abs() < 1e-6,
                    "{label}: {b0}..{b1} 拍は無音のはず、 out[{}] = {v}",
                    lo + k
                );
            }
        }
    }

    fn assert_render_matches_spans(event: &AudioEvent, source_sr: u32, label: &str) {
        assert_render_matches_spans_with(event, source_sr, None, 0.02, label);
    }

    /// 伸ばした Slice clip: trigger は広がるが slice 本体は native rate → gap が空く。
    /// **これが r.md #41 の症状そのもの** (旧描画は連続波形を全幅に引き伸ばしていた)。
    #[test]
    fn slice_spans_match_render_with_gaps() {
        let ev = slice_event(4.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        let mut spans = Vec::new();
        event_wave_spans(&ev, 48_000, &common::audio_render::TempoMap::constant(BPM), 0.0, &mut spans);
        assert_eq!(spans.len(), 4);
        assert!(
            spans[0].end_beat < spans[1].start_beat - 1e-9,
            "伸ばした Slice は gap が空く: {spans:?}"
        );
        assert_render_matches_spans(&ev, 48_000, "slice gap");
    }

    /// 詰めた Slice clip: 次 trigger が先に来て前 slice が cut される (gap 無し)。
    #[test]
    fn slice_spans_match_render_with_cuts() {
        let ev = slice_event(1.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        assert_render_matches_spans(&ev, 48_000, "slice cut");
    }

    /// 移調した Slice clip: trigger は動かず slice 本体だけ速くなる → gap。
    #[test]
    fn slice_spans_match_render_when_pitched() {
        let ev = slice_event(2.0, 12.0, vec![0, 12_000, 24_000, 36_000]);
        assert_render_matches_spans(&ev, 48_000, "slice pitch");
    }

    /// 伸縮なしの Slice clip は隙間ゼロ (= 従来の連続波形と同じ絵) で、 かつ
    /// 各 slice が正しい source を鳴らす。 取り込み直後の回帰検出。
    #[test]
    fn slice_spans_match_render_when_not_stretched() {
        let ev = slice_event(2.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        let mut spans = Vec::new();
        event_wave_spans(&ev, 48_000, &common::audio_render::TempoMap::constant(BPM), 0.0, &mut spans);
        for w in spans.windows(2) {
            assert!(
                (w[1].start_beat - w[0].end_beat).abs() < 1e-9,
                "伸縮なしなら隙間ゼロ: {spans:?}"
            );
        }
        assert_render_matches_spans(&ev, 48_000, "slice lockstep");
    }

    /// source SR ≠ engine SR でも span と実出力が一致する (SR 補正漏れの回帰検出)。
    #[test]
    fn slice_spans_match_render_at_other_sample_rate() {
        for source_sr in [44_100u32, 96_000] {
            let ev = AudioEvent {
                source_end_frames: u64::from(source_sr),
                ..slice_event(4.0, 0.0, vec![0, u64::from(source_sr) / 4, u64::from(source_sr) / 2])
            };
            assert_render_matches_spans(&ev, source_sr, &format!("slice {source_sr}Hz"));
        }
    }

    /// tape 系 (Raw) をピッチアップすると途中で鳴り終わる。 span も同じ拍で終わる。
    #[test]
    fn raw_pitched_span_matches_render() {
        let ev = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Raw,
            pitch_semitones: 12.0,
            ..AudioEvent::default()
        };
        assert_render_matches_spans(&ev, 48_000, "raw pitched");
    }

    /// **既定 mode** (`AudioEvent::default()` = `Stretch`) の uniform 写像が実出力と
    /// 一致する。 granular は grain hop で source offset が量子化されるので許容を緩める。
    #[test]
    fn stretch_uniform_span_matches_render() {
        for len_beats in [2.0_f64, 4.0, 1.0] {
            let ev = AudioEvent {
                source_start_frames: 0,
                source_end_frames: SOURCE_FRAMES,
                event_length_beats: len_beats,
                stretch_mode: StretchMode::Stretch,
                ..AudioEvent::default()
            };
            assert_render_matches_spans_with(
                &ev,
                48_000,
                None,
                0.03,
                &format!("stretch uniform {len_beats}拍"),
            );
        }
    }

    /// Stretch + warp marker の区分線形写像が実出力 (granular の warp path) と一致する。
    /// **窓手前へ外挿する marker** (= trim / split で `source_start_frames` だけが
    /// 前進した event) を含める: engine は grain ごとに `(sf - source_start).max(0)` と
    /// clamp するので窓手前は先頭 frame 保持 (flat) になり、 描画が端点 clamp + 線形補間
    /// だと別形になる (r.md #41 レビュー指摘 5)。
    #[test]
    fn stretch_warp_span_matches_render() {
        use common::model::BeatMarker;
        // 0..2 拍で source 前半 12000 frame、 2..4 拍で残り 36000 frame という非一様 warp。
        let ev = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Stretch,
            beat_markers: vec![
                BeatMarker { source_frame: 0, locked_beat: 0.0 },
                BeatMarker { source_frame: 12_000, locked_beat: 2.0 },
                BeatMarker { source_frame: 48_000, locked_beat: 4.0 },
            ],
            ..AudioEvent::default()
        };
        assert_render_matches_spans_with(&ev, 48_000, None, 0.03, "stretch warp");

        // 左 trim 相当: source_start_frames だけ前進し marker は据え置き → 拍区間の
        // **途中** で warp が窓手前から窓内へ入る。 engine は grain ごとの
        // `(sf - source_start).max(0)` なので前半 flat + 後半が本来の傾き。
        // 端点だけ clamp して線形補間すると全域で source が先走る (最大 25% ずれる)。
        let ev = AudioEvent {
            source_start_frames: 24_000,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Stretch,
            beat_markers: vec![
                BeatMarker { source_frame: 0, locked_beat: 0.0 },
                BeatMarker { source_frame: 48_000, locked_beat: 4.0 },
            ],
            ..AudioEvent::default()
        };
        assert_render_matches_spans_with(&ev, 48_000, None, 0.05, "stretch warp trimmed");
    }

    /// Repitch (tape) の uniform 写像が実出力 (累積器 path) と一致する。
    #[test]
    fn repitch_span_matches_render() {
        for semis in [0.0_f32, 12.0, -12.0] {
            let ev = AudioEvent {
                source_start_frames: 0,
                source_end_frames: SOURCE_FRAMES,
                event_length_beats: 4.0,
                stretch_mode: StretchMode::Repitch,
                pitch_semitones: semis,
                ..AudioEvent::default()
            };
            assert_render_matches_spans_with(&ev, 48_000, None, 0.02, &format!("repitch {semis}"));
        }
    }

    /// SongTempo automation 下でも描画写像と実出力が一致する (r.md #41 レビュー指摘 1)。
    /// `Raw` の消費速度と `Slice` 本体の read 速度だけが current_bpm に反比例するので、
    /// 描画が定数 `song.bpm` 固定だとここで落ちる。
    #[test]
    fn spans_match_render_under_tempo_automation() {
        // song.bpm = 120 のまま SongTempo lane で 60 BPM 一定にする
        // (= 1 拍あたりの実時間が 2 倍 → native rate は 2 倍の source を消費)。
        let song = tempo_song(60.0, 60.0, 16.0);
        let ev = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Raw,
            ..AudioEvent::default()
        };
        assert_render_matches_spans_with(&ev, 48_000, Some(&song), 0.02, "raw @tempo curve");

        let ev = AudioEvent { stretch_mode: StretchMode::Slice, onsets: vec![0, 12_000, 24_000, 36_000], ..ev };
        assert_render_matches_spans_with(&ev, 48_000, Some(&song), 0.02, "slice @tempo curve");

        // 配置しか使わない mode は tempo 曲線でも従来どおり (= 拍不変) であること。
        let ev = AudioEvent { stretch_mode: StretchMode::Stretch, onsets: Vec::new(), ..ev };
        assert_render_matches_spans_with(&ev, 48_000, Some(&song), 0.03, "stretch @tempo curve");

        // ramp (60→120 BPM) でも一致する = 区分線形化が効いている。
        let ramping = tempo_song(60.0, 120.0, 8.0);
        let ev = AudioEvent { stretch_mode: StretchMode::Raw, ..ev };
        assert_render_matches_spans_with(&ev, 48_000, Some(&ramping), 0.03, "raw @tempo ramp");
    }

    /// `beat 0..len` を `start_bpm → end_bpm` の直線で結ぶ SongTempo lane を持つ song。
    fn tempo_song(start_bpm: f64, end_bpm: f64, len_beats: f64) -> common::model::Song {
        use common::model::{
            AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
            AutomationTarget, ClipContent, Song,
        };
        let mut song = Song { bpm: BPM, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                next_point_id: 0,
                points: vec![
                    AutomationPoint { id: 0, time_beat: 0.0, value: start_bpm, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 0, time_beat: len_beats, value: end_bpm, curve: AutomationCurve::Linear },
                ],
            }),
        );
        let lane_id = song.alloc_song_lane_id();
        let mut lane = AutomationLane::new(AutomationTarget::SongTempo, f64::from(BPM));
        lane.id = lane_id;
        lane.clips.push(AutomationClip {
            id: 1,
            name: "Tempo".into(),
            start_beat: 0.0,
            length_beats: len_beats,
            content_id: cid,
        });
        lane.next_clip_id = 2;
        song.song_lanes.push(lane);
        song
    }

    /// 逆再生は窓全体を反転して読む。 span の `reversed` 写像が実出力と一致する
    /// (描画側は今まで `reversed` を一切見ておらず、 波形が嘘をついていた)。
    #[test]
    fn reversed_span_matches_render() {
        let ev = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 2.0,
            stretch_mode: StretchMode::Raw,
            reversed: true,
            ..AudioEvent::default()
        };
        assert_render_matches_spans(&ev, 48_000, "raw reversed");

        let ev = AudioEvent {
            reversed: true,
            ..slice_event(4.0, 0.0, vec![0, 12_000, 24_000, 36_000])
        };
        assert_render_matches_spans(&ev, 48_000, "slice reversed");
    }
}

/// E5 (r.md #8): granular grain-trigger lock-in。 grain の source offset が trigger 時の値に
/// 固定され、 後続 buffer で tempo_ratio が変わっても再計算されない (= tempo 変化での source
/// position 跳び = click を防ぐ) ことを検証する。
#[cfg(test)]
mod e5_lockin_tests {
    use super::*;

    const HOP: u64 = 512; // GRAIN_HOP_SAMPLES と一致 (granular_sample_at 内 const)。

    fn call(el: u64, ratio: f64, ring: Option<&mut GrainLockRing>) {
        let plane = vec![0.0f32; 200_000];
        let _ = granular_sample_at(
            el, ratio, 1.0, 1.0, &plane, &plane, 0, 200_000, 200_000, &[], 512.0, false, ring,
        );
    }

    #[test]
    fn grain_offset_is_locked_at_trigger_not_recomputed() {
        let mut ring: GrainLockRing = [(u64::MAX, 0); 8];
        let k = 3u64;
        let event_local = k * HOP + HOP / 2; // grain k が active (mid-window)。
        let slot = (k as usize) % ring.len();

        // trigger (ratio 1.0): grain k の offset = k*HOP*1.0 が記録される。
        call(event_local, 1.0, Some(&mut ring));
        assert_eq!(ring[slot].0, k, "grain k が ring に記録される");
        let locked = ring[slot].1;
        assert_eq!(locked, k * HOP, "trigger offset = k*HOP*1.0");

        // 同じ出力位置で tempo_ratio を倍にしても offset は固定 (recompute なら k*HOP*2.0)。
        call(event_local, 2.0, Some(&mut ring));
        assert_eq!(
            ring[slot].1, locked,
            "grain offset は trigger 時に lock され tempo 変化で再計算されない"
        );
    }

    #[test]
    fn stale_slot_is_overwritten_for_a_new_grain() {
        // seek 相当: 同じ slot に別 grain (k+8) が来たら k 不一致で現 ratio で上書き (自己無効化)。
        let mut ring: GrainLockRing = [(u64::MAX, 0); 8];
        let k = 3u64;
        let slot = (k as usize) % ring.len();
        call(k * HOP + HOP / 2, 1.0, Some(&mut ring));
        assert_eq!(ring[slot], (k, k * HOP));

        let k2 = k + 8; // 同じ slot (k2 % 8 == k % 8)。
        call(k2 * HOP + HOP / 2, 3.0, Some(&mut ring));
        assert_eq!(ring[slot].0, k2, "k 不一致の slot は新 grain で上書きされる");
        assert_eq!(ring[slot].1, k2 * HOP * 3, "上書きは現 ratio で recompute");
    }

    #[test]
    fn none_ring_keeps_legacy_recompute_path() {
        // lock_ring=None は従来挙動 (毎回 recompute) で panic しない。
        call(3 * HOP + HOP / 2, 1.0, None);
        call(3 * HOP + HOP / 2, 2.0, None);
    }

    #[test]
    fn repitch_integrates_position_continuously_across_tempo_change() {
        // E5 sibling: contiguous 再生で ratio を積分 → tempo 変化でも位置が連続 (跳ばない)。
        let mut state = (u64::MAX, 0.0);
        for el in 0..4u64 {
            let p = repitch_source_pos(&mut state, el, 1.0);
            assert!((p - el as f64).abs() < 1e-9, "ratio 1.0 で 位置 == event_local");
        }
        // event_local 4 で ratio が 2.0 に変化 (tempo automation)。 連続なので 3.0 + 2.0 = 5.0。
        // 旧実装の `event_local × ratio` なら 4×2 = 8.0 に跳ぶ (= click)。
        let p4 = repitch_source_pos(&mut state, 4, 2.0);
        assert!((p4 - 5.0).abs() < 1e-9, "連続積分 3.0+2.0=5.0 で跳ばない、 got {p4}");
        let p5 = repitch_source_pos(&mut state, 5, 2.0);
        assert!((p5 - 7.0).abs() < 1e-9, "5.0+2.0=7.0");

        // seek (event_local 不連続) → 現 ratio で再 anchor (event_local × ratio)。
        let p_seek = repitch_source_pos(&mut state, 100, 2.0);
        assert!((p_seek - 200.0).abs() < 1e-9, "seek は現 ratio で再 anchor 100×2=200");
    }

    #[test]
    fn repitch_constant_ratio_matches_legacy_formula() {
        // Raw 相当 (ratio 一定) では積分値 == event_local × ratio (従来挙動と byte 一致)。
        let mut state = (u64::MAX, 0.0);
        for el in 0..10u64 {
            let p = repitch_source_pos(&mut state, el, 1.5);
            assert!(
                (p - el as f64 * 1.5).abs() < 1e-9,
                "constant ratio は el×1.5、 got {p} at el={el}"
            );
        }
    }
}
