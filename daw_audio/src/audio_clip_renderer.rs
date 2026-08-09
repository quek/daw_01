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
    clamp_semitones, FORMANT_SEMITONES_LIMIT, PITCH_SEMITONES_LIMIT,
};

use crate::stretch_engine::StretchEngine;

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
/// - **Stretch: スペクトル time-stretch が tempo に追従** (pitch / formant を
///   独立に持つ、 `crate::stretch_engine::StretchEngine`。 r.md #40 で granular
///   OLA から置換 — granular は原理的にピッチとフォルマントを分離できない)
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
    /// frame へ写す全経路 (Raw/Repitch の stride、 Stretch の中間ストリーム間隔、
    /// slice の trigger 写像) に掛かる。 pitch とは独立。
    pub sr_ratio: f64,
    /// **ピッチ軸**の比 (= `2^(semitones/12)`)。 tape 系 (Raw / Repitch) は source を
    /// 読む速度そのものに掛かる (長さも変わる)。 Slice は slice の **内部読み出し**に
    /// だけ掛かり **配置**には掛からない (= 長さを変えずに移調)。 Stretch は
    /// スペクトルエンジンが `pitch_semitones` を直接受けるのでこの比は使わない。
    pub pitch_factor: f64,
    /// 移調量 (半音、clamp 済)。 Stretch (スペクトル経路) はこちらを使う
    /// (`pitch_factor` は tape / slice 用)。
    pub pitch_semitones: f32,
    /// スペクトル包絡 (フォルマント) の移調量 (半音、clamp 済)。 r.md #40。
    /// `0.0` かつ tape / slice mode なら DSP は完全バイパスされる。
    pub formant_semitones: f32,
    /// 発音の安定キー (`clip.id` << 32 | `AudioEvent.id`)。 stretch engine が
    /// 「同じ発音の続きか」 を判定するのに使う (= positional index を使わない、
    /// アーキ不変条件 #1)。 編集で schedule を組み直しても値が変わらないので、
    /// 無関係な編集で発音中の clip が re-prime されない。
    pub stream_key: u64,
    /// この event が使う per-track stretch engine の slot (= track 内で beat 区間が
    /// 重なる event 同士が別 slot になるよう off-RT で貪欲彩色した結果)。
    /// `None` = エンジン不要 (tape / slice + formant 0) か、pool 上限超過。
    pub engine_slot: Option<u16>,
    /// source の native 長 / event の配置長 の比
    /// (= `native_secs / event_secs`、 nominal bpm 基準)。 `1.0` で「source を
    /// そのまま」 (= trim、 native rate)。 `< 1.0` で event slot の方が長い → source
    /// を引き伸ばす (slow)、 `> 1.0` で詰める (fast)。 Repitch は pitch にも乗る
    /// (tape)、 Stretch はスペクトル経路で pitch 保持、 Slice は slice 配置にのみ作用、
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
    /// dedup 済)。 Stretch mode のスペクトル render が `warp_source_frame` で
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
    /// r.md #40: track index → その track が必要とする
    /// [`crate::stretch_engine::StretchEngine`] の数 (= `engine_slot` の最大値+1
    /// = beat 区間が重なる Stretch / formant≠0 event の最大同時数)。
    /// **RT では確保できない**ので、off-thread の publish 側がこれを見て不足分の
    /// エンジンを作り `TrackScratch::stretch_engines` へ配送する。
    pub engines_per_track: Vec<u16>,
}

impl AudioClipRenderer {
    pub fn empty() -> Self {
        Self {
            schedule: Vec::new(),
            sources: HashMap::new(),
            engines_per_track: Vec::new(),
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
            for (event_seq, event) in audio.events.iter().enumerate() {
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
                let pitch_semitones =
                    clamp_semitones(event.pitch_semitones, PITCH_SEMITONES_LIMIT);
                let formant_semitones =
                    clamp_semitones(event.formant_semitones, FORMANT_SEMITONES_LIMIT);
                let pitch_factor = pitch_factor(pitch_semitones);
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
                    pitch_semitones,
                    formant_semitones,
                    // 安定 id で発音を識別する。 `AudioEvent.id` が未採番 (0) の
                    // 古い project では content 内の位置で代用する (load 時に
                    // `ensure_*_ids` が採番するので通常は通らない fallback)。 実 id は
                    // 1 から順に採番されるので最上位ビットは常に 0 — fallback をそこに
                    // 逃がして、採番済み event との衝突を構造的に無くす。
                    stream_key: (u64::from(clip.id) << 32)
                        | u64::from(if event.id != 0 {
                            event.id
                        } else {
                            0x8000_0000 | u32::try_from(event_seq).unwrap_or(0x7fff_ffff)
                        }),
                    // 彩色は schedule 完成後 (`assign_engine_slots`)。
                    engine_slot: None,
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
    let engines_per_track = assign_engine_slots(&mut schedule);
    tracing::info!(
        n_events = schedule.len(),
        n_sources = sources.len(),
        engine_sr = engine_sample_rate,
        bpm = song.bpm,
        n_engines = engines_per_track.iter().map(|&n| usize::from(n)).sum::<usize>(),
        "compiled audio schedule"
    );
    AudioClipRenderer {
        schedule,
        sources,
        engines_per_track,
    }
}

/// 1 track が同時に持てる stretch engine の数。1 個あたり ~1 MB (STFT 120 ms /
/// 2ch) 使うので、`MAX_GRANULAR_EVENTS_PER_TRACK` (256) 相当を確保すると破綻する。
/// 「1 track で **同時に鳴っている** Stretch / formant≠0 の event 数」が上限なので、
/// クロスフェード込みでも実用上 2〜4。32 は十分な余裕。溢れた event はエンジン
/// 無しに degrade する (`render_audio_events` の fallback を参照)。
pub const MAX_STRETCH_ENGINES_PER_TRACK: usize = 32;

/// この event はスペクトルエンジンを要るか。
/// - `Stretch`: 常に要る (時間伸縮 + 移調 + フォルマントを一括で担う)
/// - tape / slice: `formant_semitones != 0` のときだけ (0 は完全バイパスで、
///   出力が 1 サンプルも変わらないことを保証する)
fn needs_stretch_engine(ev: &RenderedEvent) -> bool {
    ev.stretch_mode == StretchMode::Stretch || ev.formant_semitones != 0.0
}

/// track ごとに「beat 区間が重なる event 同士は別 slot」となるよう engine slot を
/// 貪欲彩色し、track index → 必要エンジン数を返す。off-RT (compile 時) 専用。
///
/// 区間グラフの貪欲彩色なので使う色数 = **最大同時発音数** = 最適。 `schedule` は
/// `start_beat` 昇順である前提 (呼び出し元が sort 済)。 同じ event 集合に対しては
/// 決定的なので、無関係な編集で slot が入れ替わって re-prime が走ることもない。
fn assign_engine_slots(schedule: &mut [RenderedEvent]) -> Vec<u16> {
    // track index → slot ごとの「現在その slot を占有している event の end_beat」。
    let mut per_track: HashMap<usize, Vec<f64>> = HashMap::new();
    let mut max_track = 0usize;
    for ev in schedule.iter_mut() {
        max_track = max_track.max(ev.track_idx);
        if !needs_stretch_engine(ev) {
            ev.engine_slot = None;
            continue;
        }
        let slots = per_track.entry(ev.track_idx).or_default();
        // 既に空いている (= end_beat <= この event の start_beat) 最小 slot を再利用。
        let slot = match slots.iter().position(|&end| end <= ev.start_beat) {
            Some(i) => i,
            None if slots.len() < MAX_STRETCH_ENGINES_PER_TRACK => {
                slots.push(f64::NEG_INFINITY);
                slots.len() - 1
            }
            // 上限超過: エンジン無しで degrade (下の fallback 経路)。
            None => {
                ev.engine_slot = None;
                continue;
            }
        };
        slots[slot] = ev.end_beat;
        ev.engine_slot = u16::try_from(slot).ok();
    }
    let mut per_track_counts = vec![0u16; if schedule.is_empty() { 0 } else { max_track + 1 }];
    for (track, slots) in per_track {
        if let Some(slot) = per_track_counts.get_mut(track) {
            *slot = u16::try_from(slots.len()).unwrap_or(u16::MAX);
        }
    }
    per_track_counts
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

/// `render_audio_events` が使う per-track の **可変** 状態への参照束。
/// 実体は `TrackScratch` にあり (= RT で確保しない)、export は自前の
/// `TrackScratch` 配列を使うので live / offline で同じ経路を通る (不変条件 #6)。
pub struct ClipRenderState<'a> {
    /// tape (Raw / Repitch) mode の連続 source 位置 accumulator (event 単位、
    /// 添字 = track 内 schedule 順)。 `(last_event_local, accumulated_source)`。
    /// E5 (r.md #8): tempo 変化で `event_local × ratio` の絶対位置が跳ぶ click を
    /// 防ぐため、contiguous 再生では ratio を積分する。
    pub repitch_accum: &'a mut [(u64, f64)],
    /// r.md #40: per-track の stretch engine pool (添字 = `RenderedEvent::engine_slot`)。
    /// off-thread で確保され `TrackScratch` に配送される。
    pub engines: &'a mut [StretchEngine],
    /// エンジン経路の per-event 出力バッファ (`MAX_FRAMES`)。 fade / gain / pan を
    /// 掛ける前の素の DSP 出力を受ける。
    pub event_l: &'a mut [f32],
    pub event_r: &'a mut [f32],
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
    state: &mut ClipRenderState<'_>,
) {
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let n = frames as usize;
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    let buf_end_beats =
        playhead_beats + f64::from(frames) / samples_per_beat;

    // E5 (r.md #8): track 内 event を schedule 順に数える安定 index (repitch
    // accumulator の添字)。 track_idx filter 後・overlap skip 前に増やすので、
    // 同じ event は buffer を跨いで同じ index になる。
    let mut track_event_seq = 0usize;
    for event in &renderer.schedule {
        if event.track_idx != track_idx {
            continue;
        }
        let accum_idx = track_event_seq;
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
        // Repitch / Slice は instant bpm を使う (= pitch / slice trigger の追随性優先)。
        // Stretch は **beat 領域**で写像するのでここは通らない (下記
        // `src_frames_per_beat` — tempo に依らない量なので LP smoothing 自体が不要に
        // なった。 r.md #40 で granular を退役させたときに smoothed bpm 経路ごと撤去)。
        // nominal_bpm は per-event の compile時 song.bpm なので、 base bpm を変えても
        // 追従する (= 旧実装の Stretch は current/song.bpm 駆動で追従しなかった)。
        let nominal_bpm = f64::from(event.nominal_bpm);
        let follow_instant =
            tempo_follow_ratio(event.stretch_ratio, f64::from(current_bpm), nominal_bpm);
        // 「出力 sample → source frame」 の **時間軸** 換算 (= SR 比)。 退化値
        // (0 / 負 / NaN) は 1.0 に倒す defensive (source が止まって drone 化しない)。
        // ※ この下の mode 別合成 (time_stride / read_stride) は、 波形描画側の
        // `common::audio_render::audible_source_span` と同じ方針でなければならない
        // (= 描いた波形と鳴る音が一致する条件)。 片方だけ変えないこと。
        let time_stride = if event.sr_ratio > 0.0 { event.sr_ratio } else { 1.0 };
        // source を **読む** 速度 = 時間軸 × ピッチ軸。 slice はこれを slice の
        // 内部読み出しにだけ使い、 配置には `time_stride` を使うので、 移調しても
        // 長さが変わらない。
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

        // --- 出力範囲を event-local に揃える ------------------------------
        // event_local = i - event_start_offset_in_buf。 負の区間 (= event 開始前)
        // は鳴らさないので、開始 offset をここで前詰めする。
        #[allow(clippy::cast_sign_loss)]
        let first_i = buf_off_start.max(event_start_offset_in_buf.max(0) as usize);
        if first_i >= buf_off_end {
            continue;
        }
        #[allow(clippy::cast_sign_loss)]
        let el_start = (first_i as i64 - event_start_offset_in_buf) as u64;
        let count = (buf_off_end - first_i).min(state.event_l.len()).min(state.event_r.len());

        // r.md #40: この event に割り当てられたスペクトルエンジン。
        // - `Stretch`: 時間伸縮 + 移調 + フォルマントを 1 段で担う (granular を置換)
        // - tape / slice + formant≠0: 素の DSP 出力に **フォルマントだけ**掛ける後段
        //
        // `None` = formant 0 の tape / slice (= 完全バイパスで出力が 1 サンプルも
        // 変わらない) か、pool 上限超過 / 配送待ちの degrade。 degrade した Stretch は
        // 下の tape 経路を **ピッチ比なし**の伸縮率で通るので、長さと拍同期は保たれ、
        // 伸縮率 1.0 近傍 (= 大多数) では正しい出力と一致する。
        let engine = event
            .engine_slot
            .map(usize::from)
            .and_then(|slot| state.engines.get_mut(slot));

        if let Some(engine) = engine {
            let out_l = &mut state.event_l[..count];
            let out_r = &mut state.event_r[..count];
            match event.stretch_mode {
                StretchMode::Stretch => {
                    // 時間写像は **beat 領域**で持つ。 「1 拍あたり消費する source
                    // frame 数」 = `source_sr * 60 / nominal_bpm * stretch_ratio` は
                    // **tempo に依らない**ので、tempo automation でも source 位置が
                    // 跳ばず (= 旧 granular の grain lock-in ring と LP smoothed bpm が
                    // 不要になった)、かつ拍にロックしたまま追従する。 波形描画側
                    // (`audible_source_span` の `source_frames_per_beat * rate`) と
                    // 同一の量で、「描いた波形 = 鳴る音」 が保たれる。
                    let src_frames_per_beat = if nominal_bpm > 0.0 {
                        f64::from(buffer.sample_rate) * 60.0 / nominal_bpm * event.stretch_ratio
                    } else {
                        0.0
                    };
                    // この buffer の先頭出力サンプルに対応する event-local beat。
                    // playhead_beats は engine が積分した真の拍位置なので、buffer を
                    // 跨いでも tempo が変わっても連続。
                    let first_beat =
                        playhead_beats + first_i as f64 / samples_per_beat - event.start_beat;
                    let warped = event.beat_markers.len() >= 2;
                    let u_of = |el: u64| -> f64 {
                        let beat = first_beat
                            + el.saturating_sub(el_start) as f64 / samples_per_beat;
                        if warped
                            && let Some(sf) =
                                common::audio_render::warp_source_frame(beat, &event.beat_markers)
                        {
                            // warp marker は source frame を beat に pin するので、
                            // 戻り値は絶対 source frame。 event 窓の起点へ寄せる。
                            return sf - event.source_start_frames as f64;
                        }
                        beat * src_frames_per_beat
                    };
                    engine.render(
                        event.stream_key,
                        event.pitch_semitones,
                        event.formant_semitones,
                        // formant 0 でも「移調中はスペクトル包絡を据え置く」。
                        // これが r.md #40 の依頼そのもの (= Ableton Complex Pro の
                        // Formants=100% / Cubase VariAudio / Melodyne 流)。
                        true,
                        el_start,
                        time_stride,
                        u_of,
                        |u| {
                            source_frame_lerp(
                                l_plane,
                                r_plane,
                                event.source_start_frames,
                                event.source_end_frames,
                                buffer.frames,
                                source_len,
                                u,
                                event.reversed,
                            )
                            .unwrap_or((0.0, 0.0))
                        },
                        out_l,
                        out_r,
                    );
                }
                mode => {
                    // テープ / slice の素の出力を 1:1 (`du = 1`、移調 0) で食わせ、
                    // スペクトル包絡だけを動かす。 エンジンは `formantMultiplier != 1`
                    // で包絡処理だけを走らせる (= 音程も長さも触らない)。
                    // accumulator は borrow 衝突を避けるためコピーして使い、後で書き戻す。
                    let mut accum = state.repitch_accum.get(accum_idx).copied();
                    engine.render(
                        event.stream_key,
                        0.0,
                        event.formant_semitones,
                        false,
                        el_start,
                        1.0,
                        |el| el as f64,
                        |u| {
                            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                            let event_local = u as u64;
                            if mode == StretchMode::Slice {
                                slice_sample_at(
                                    event_local,
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
                                )
                            } else {
                                tape_sample_at(
                                    event_local,
                                    effective_pitch_ratio,
                                    l_plane,
                                    r_plane,
                                    event.source_start_frames,
                                    event.source_end_frames,
                                    buffer.frames,
                                    source_len,
                                    event.reversed,
                                    accum.as_mut(),
                                )
                            }
                        },
                        out_l,
                        out_r,
                    );
                    if let Some(value) = accum
                        && let Some(slot) = state.repitch_accum.get_mut(accum_idx)
                    {
                        *slot = value;
                    }
                }
            }

            // fade / gain / pan は DSP 経路に依らず同じ順序で掛ける
            // (= 従来の per-sample ループと同一の演算)。
            for k in 0..count {
                let event_local = el_start + k as u64;
                let fade_in = fade_envelope(event_local, fade_in_samples, event.fade_in_curve);
                let tail = event_total_samples.saturating_sub(event_local + 1);
                let fade_out = fade_envelope(tail, fade_out_samples, event.fade_out_curve);
                let env = fade_in * fade_out * event.gain_lin;
                if env == 0.0 {
                    continue;
                }
                let i = first_i + k;
                track_l[i] += out_l[k] * env * pan_l * std::f32::consts::SQRT_2;
                track_r[i] += out_r[k] * env * pan_r * std::f32::consts::SQRT_2;
            }
            continue;
        }

        // --- エンジン不要 / 不在: 従来の per-sample テープ経路 ---------------
        // formant 0 の Raw / Repitch / Slice はここを通り、出力は r.md #40 前と
        // **1 サンプルも変わらない**。
        let mut repitch_state = state.repitch_accum.get_mut(accum_idx);
        // degrade した Stretch は「ピッチ比を掛けない伸縮率」で読む (= 長さと拍
        // 同期は保たれる)。 伸縮率 1.0 (= clip が project tempo と一致) では
        // 正しい出力と一致するので、実用上ほぼ無害。
        let tape_ratio = if event.stretch_mode == StretchMode::Stretch {
            time_stride * follow_instant
        } else {
            effective_pitch_ratio
        };

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

            // stretch_mode ごとに source sample 取得経路を分岐する。
            // - Raw / Repitch: 直接 source を読む (linear interp、 Repitch は
            //   effective_pitch_ratio に tempo_ratio 込み)
            // - Slice: transient slicing。 onset (event.onsets) を slice trigger に
            //   slice_sample_at が tempo 追従再生 (= onset 自動検出は r.md #8 B1 で実装)
            // - Stretch: 本来はスペクトル経路。 ここに来るのは engine 不在の
            //   degrade だけで、`tape_ratio` (ピッチ比なしの伸縮率) で読む。
            let (s_l, s_r) = if event.stretch_mode == StretchMode::Slice {
                slice_sample_at(
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
                )
            } else {
                tape_sample_at(
                    event_local,
                    tape_ratio,
                    l_plane,
                    r_plane,
                    event.source_start_frames,
                    event.source_end_frames,
                    buffer.frames,
                    source_len,
                    event.reversed,
                    repitch_state.as_deref_mut(),
                )
            };

            track_l[i] += s_l * env * pan_l * std::f32::consts::SQRT_2;
            track_r[i] += s_r * env * pan_r * std::f32::consts::SQRT_2;
        }
    }
}

/// tape 系 (Raw / Repitch、および engine 不在で degrade した Stretch) の
/// 1 サンプル読み出し。 `ratio` は「出力 1 sample あたり進む source frame 数」。
///
/// E5 (r.md #8): `state` を渡すと source 位置を **積分**する。 contiguous 再生
/// (`event_local == last + 1`) では ratio を足し込み、tempo automation で ratio が
/// 変わっても絶対位置が跳ばない (= click 防止)。 不連続 (seek / schedule 変化 /
/// 初回) では現 ratio で `event_local × ratio` に再 anchor する。 `None` は
/// 従来どおり毎回 `event_local × ratio` (= ratio 一定なら積分値と一致)。
///
/// 範囲外 (負 / source 窓外 / buffer 外) は `(0.0, 0.0)`。
/// RT 安全: 確保なし・panic なし。
#[inline]
#[allow(clippy::too_many_arguments)]
fn tape_sample_at(
    event_local: u64,
    ratio: f64,
    l_plane: &[f32],
    r_plane: &[f32],
    source_start: u64,
    source_end: u64,
    buffer_frames: u64,
    source_len: u64,
    reversed: bool,
    state: Option<&mut (u64, f64)>,
) -> (f32, f32) {
    let source_pos = match state {
        Some(state) => repitch_source_pos(state, event_local, ratio),
        None => event_local as f64 * ratio,
    };
    let source_pos = if reversed {
        source_len as f64 - 1.0 - source_pos
    } else {
        source_pos
    };
    // NaN も弾く (退化 ratio の伝播)。
    if source_pos < 0.0 || source_pos.is_nan() {
        return (0.0, 0.0);
    }
    let i0 = source_pos.floor() as i64;
    let frac = (source_pos - i0 as f64) as f32;
    if i0 < 0 {
        return (0.0, 0.0);
    }
    #[allow(clippy::cast_sign_loss)]
    let abs_idx0 = source_start + i0 as u64;
    let abs_idx1 = abs_idx0 + 1;
    if abs_idx0 >= source_end || abs_idx0 >= buffer_frames {
        return (0.0, 0.0);
    }
    let s_l0 = l_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
    let s_r0 = r_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
    let s_l1 = l_plane.get(abs_idx1 as usize).copied().unwrap_or(s_l0);
    let s_r1 = r_plane.get(abs_idx1 as usize).copied().unwrap_or(s_r0);
    (s_l0 + (s_l1 - s_l0) * frac, s_r0 + (s_r1 - s_r0) * frac)
}

/// event-local な **小数** source 位置 `pos_in_event` (= `source_start` 起点の
/// source frame) から linear interpolation で 1 frame 取り出す。 Stretch /
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

/// clip 再生の外形仕様 (時間写像 / 長さ / mode 別のピッチの効き方 / フォルマント
/// 配線) を、live と export が共有する `render_audio_events` 越しに検証する。
///
/// r.md #40 で Stretch の DSP を granular からスペクトル方式へ置き換えたので、
/// grain 内部挙動 (hop / lock-in ring) に依存していた旧テストは廃し、
/// **外から観測できる契約**だけを固定する。DSP そのものの性質 (移調で包絡が
/// 動かない等) は `crate::stretch_engine` 側のテストが担う。
#[cfg(test)]
mod render_tests {
    use super::*;

    const ENGINE_SR: u32 = 48_000;
    const BPM: f32 = 120.0;
    /// 4 拍 @ 120 BPM = 2 秒 (= 1 秒素材の 2 倍に stretch)。
    const LEN_BEATS: f64 = 4.0;

    /// 1 秒ぶんの ramp (0.0 → 1.0) 素材。 出力値がそのまま「source のどこを
    /// 読んでいるか」 を表すので、 テープ経路の時間写像を直接 assert できる。
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

    /// 1 秒ぶんの 440 Hz サイン × 直線エンベロープ (0.0 → 1.0)。
    /// **スペクトル経路の時間写像を測るための素材**。 素の ramp (= ほぼ DC) は
    /// 位相ボコーダの位相ランダム化 (伸縮比 2 倍以上で作動) で値が揺れて指標に
    /// ならないが、 振幅エンベロープは解析位置にそのまま追従するので、
    /// 「出力 f 地点の音量 = source f 地点の音量」 で時間写像を直接検算できる。
    fn ramped_sine_source(source_sr: u32) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                let env = i as f32 / (frames - 1) as f32;
                let phase = std::f64::consts::TAU * 440.0 * i as f64 / f64::from(source_sr);
                (phase.sin() as f32) * env
            })
            .collect();
        AudioSourceBuffer {
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    /// `center` 周りの局所 RMS (= その地点の音量)。
    fn local_rms(x: &[f32], center: usize, half_width: usize) -> f64 {
        let lo = center.saturating_sub(half_width);
        let hi = (center + half_width).min(x.len());
        if hi <= lo {
            return 0.0;
        }
        let sum: f64 = x[lo..hi].iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        (sum / (hi - lo) as f64).sqrt()
    }

    /// 1 秒ぶんの正弦波素材 (フォルマント / 移調の配線確認用)。
    fn sine_source(source_sr: u32, freq: f64) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                (std::f64::consts::TAU * freq * i as f64 / f64::from(source_sr)).sin() as f32 * 0.5
            })
            .collect();
        AudioSourceBuffer {
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    fn render_clip(source_sr: u32, mode: StretchMode, onsets: Vec<u64>) -> Vec<f32> {
        render_clip_pitched(source_sr, mode, onsets, 0.0)
    }

    fn render_clip_pitched(
        source_sr: u32,
        mode: StretchMode,
        onsets: Vec<u64>,
        semitones: f32,
    ) -> Vec<f32> {
        render_clip_full(ramp_source(source_sr), mode, onsets, semitones, 0.0)
    }

    /// clip 全長を 512 frame ずつ render して L channel を連結する。
    /// engine pool は `assign_engine_slots` が出した必要数を off-RT で確保する
    /// (= live の publish 経路 / export の walk と同じ手順)。
    fn render_clip_full(
        buffer: AudioSourceBuffer,
        mode: StretchMode,
        onsets: Vec<u64>,
        semitones: f32,
        formant: f32,
    ) -> Vec<f32> {
        let source_sr = buffer.sample_rate;
        let source_frames = buffer.frames;
        let mut schedule = vec![RenderedEvent {
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
            pitch_semitones: semitones,
            formant_semitones: formant,
            stream_key: 1,
            engine_slot: None,
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
        }];
        let engines_per_track = assign_engine_slots(&mut schedule);
        let mut sources = HashMap::new();
        sources.insert(1u32, Arc::new(buffer));
        let renderer = AudioClipRenderer {
            schedule,
            sources,
            engines_per_track,
        };

        let n_engines = renderer.engines_per_track.first().copied().unwrap_or(0);
        let mut engines: Vec<StretchEngine> = (0..n_engines)
            .map(|_| StretchEngine::new(ENGINE_SR).expect("stretch engine"))
            .collect();

        let samples_per_beat = f64::from(ENGINE_SR) * 60.0 / f64::from(BPM);
        let total = (LEN_BEATS * samples_per_beat) as usize;
        let mut accum = vec![(u64::MAX, 0.0f64); 4];
        let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
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
                &mut ClipRenderState {
                    repitch_accum: &mut accum,
                    engines: &mut engines,
                    event_l: &mut event_l,
                    event_r: &mut event_r,
                },
            );
            out.extend_from_slice(&l);
        }
        // pan 中央 (equal-power × √2) は利得 1.0 に戻る前提を固定する。
        out
    }

    /// Goertzel: `freq` 成分の振幅。
    fn magnitude_at(x: &[f32], freq: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / f64::from(ENGINE_SR);
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = f64::from(v) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / x.len() as f64
    }

    /// 最後に音が出ている出力 frame (= source を使い切った位置)。
    fn last_audible(out: &[f32]) -> usize {
        out.iter().rposition(|s| s.abs() > 1e-3).unwrap_or(0)
    }

    /// 出力の位置 `f` (0..1) で source の位置 `f` が鳴っている = 時間写像が正しい。
    /// 44.1 kHz 素材を 48 kHz engine で鳴らしても clip の端まで音が続く
    /// (= 「波形より音が短い」 の回帰検出)。 スペクトル経路に置き換えても
    /// **時間写像の外形は不変**であることを固定する。
    #[test]
    fn stretch_maps_output_time_to_source_time_at_any_sample_rate() {
        // 振幅 0→1 の直線エンベロープなので、 出力 f 地点の RMS は
        // `f * (1/sqrt(2))` になるはず (サイン波の RMS = 振幅/sqrt(2))。
        let full_scale = 1.0 / std::f64::consts::SQRT_2;
        for source_sr in [48_000u32, 44_100, 96_000] {
            let out = render_clip_full(
                ramped_sine_source(source_sr),
                StretchMode::Stretch,
                Vec::new(),
                0.0,
                0.0,
            );
            let total = out.len() as f64;
            for f in [0.25_f64, 0.5, 0.75, 0.95] {
                let idx = (total * f) as usize;
                let got = local_rms(&out, idx, 2_000) / full_scale;
                assert!(
                    (got - f).abs() < 0.06,
                    "source {source_sr} Hz: 出力 {f} 地点で source {f} 地点が鳴るべき、 got {got}"
                );
            }
            // 末尾がまるごと無音になっていないこと (= 「波形より音が短い」 の回帰検出)。
            let tail = local_rms(&out, (total * 0.99) as usize, 2_000) / full_scale;
            assert!(
                tail > 0.8,
                "source {source_sr} Hz: clip 末尾まで鳴るべき、 got {tail}"
            );
        }
    }

    /// テープ経路 (Raw) の時間写像は素の ramp で直接読める (= 値がそのまま
    /// source 位置)。 SR 比を落とすと source を速く消費して末尾が無音になる。
    #[test]
    fn raw_reads_source_at_native_rate_at_any_sample_rate() {
        for source_sr in [48_000u32, 44_100, 96_000] {
            let out = render_clip(source_sr, StretchMode::Raw, Vec::new());
            // Raw は伸縮しないので 1 秒素材は出力 2 秒のうち前半で鳴り終わる。
            let one_second = ENGINE_SR as usize;
            for f in [0.25_f64, 0.5, 0.75, 0.95] {
                let idx = (one_second as f64 * f) as usize;
                let got = out[idx];
                assert!(
                    (f64::from(got) - f).abs() < 0.01,
                    "source {source_sr} Hz: 出力 {f} 秒地点で source {f} 地点が鳴るべき、 got {got}"
                );
            }
        }
    }

    /// Stretch で移調しても **長さは変わらない** (= 時間伸縮と移調が直交)。
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

    /// Stretch のピッチ指定が実際に基本周波数を動かす (= inspector 配線の確認)。
    #[test]
    fn pitch_shift_moves_the_pitch_in_stretch_mode() {
        let out = render_clip_full(
            sine_source(48_000, 440.0),
            StretchMode::Stretch,
            Vec::new(),
            12.0,
            0.0,
        );
        // 過渡を避けて中央付近を測る。
        let mid = &out[out.len() / 3..out.len() * 2 / 3];
        let m880 = magnitude_at(mid, 880.0);
        let m440 = magnitude_at(mid, 440.0);
        assert!(
            m880 > m440 * 4.0,
            "Stretch +12 半音で 880 Hz が主成分になるべき: 880={m880} 440={m440}"
        );
    }

    /// tape 系 (Raw / Repitch) と Slice は、 移調がそのまま再生速度になる
    /// (= +1 oct で source を 2 倍速で消費 → 鳴る長さが半分)。
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
        let onsets = vec![0u64, 22_050];
        let out = render_clip(source_sr, StretchMode::Slice, onsets);

        assert!(out[23_000] > 0.4, "slice 0 の末尾直前は鳴っている: {}", out[23_000]);
        assert!(
            out[30_000].abs() < 1e-6,
            "slice 0 終了後・slice 1 trigger 前は gap: {}",
            out[30_000]
        );
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

    // ---- r.md #40: フォルマント ------------------------------------------

    fn test_event(mode: StretchMode, formant: f32, start: f64, end: f64) -> RenderedEvent {
        RenderedEvent {
            track_idx: 0,
            clip_idx: 0,
            start_beat: start,
            end_beat: end,
            source_id: 1,
            source_start_frames: 0,
            source_end_frames: 48_000,
            gain_lin: 1.0,
            pan: 0.0,
            sr_ratio: 1.0,
            pitch_factor: 1.0,
            pitch_semitones: 0.0,
            formant_semitones: formant,
            stream_key: 1,
            engine_slot: None,
            stretch_ratio: 1.0,
            nominal_bpm: BPM,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: false,
            stretch_mode: mode,
            onsets: Vec::new(),
            beat_markers: Vec::new(),
        }
    }

    /// tape / slice + `formant == 0` は **エンジンを使わない** (= DSP 完全
    /// バイパス、出力が 1 サンプルも変わらない契約)。 Stretch は常に使う。
    #[test]
    fn engine_is_required_only_where_formant_can_act() {
        let cases = [
            (StretchMode::Raw, 0.0f32, false),
            (StretchMode::Repitch, 0.0, false),
            (StretchMode::Slice, 0.0, false),
            (StretchMode::Stretch, 0.0, true),
            (StretchMode::Raw, 3.0, true),
            (StretchMode::Slice, -3.0, true),
            (StretchMode::Stretch, 3.0, true),
        ];
        for (mode, formant, want) in cases {
            let mut schedule = vec![test_event(mode, formant, 0.0, 4.0)];
            let per_track = assign_engine_slots(&mut schedule);
            assert_eq!(
                schedule[0].engine_slot.is_some(),
                want,
                "{mode:?} formant={formant}"
            );
            assert_eq!(per_track.first().copied().unwrap_or(0), u16::from(want));
        }
    }

    /// slot は **重なった event の数**だけ使い、離れた event は使い回す
    /// (= 区間グラフの貪欲彩色 = 最大同時発音数が必要数)。 これを誤ると
    /// track あたり数百 MB のエンジンを確保して破綻する。
    #[test]
    fn engine_slots_reuse_across_non_overlapping_events() {
        let mut schedule = vec![
            test_event(StretchMode::Stretch, 0.0, 0.0, 4.0),
            // 重なる → 別 slot
            test_event(StretchMode::Stretch, 0.0, 2.0, 6.0),
            // 上 2 つが終わってから → slot 0 を再利用
            test_event(StretchMode::Stretch, 0.0, 8.0, 12.0),
        ];
        let per_track = assign_engine_slots(&mut schedule);
        assert_eq!(schedule[0].engine_slot, Some(0));
        assert_eq!(schedule[1].engine_slot, Some(1));
        assert_eq!(schedule[2].engine_slot, Some(0), "非重複は slot 再利用");
        assert_eq!(per_track, vec![2], "同時発音数 = 2 個で足りる");
    }

    /// 上限を超える同時発音は `None` に落ち、エンジン無しで degrade する
    /// (= 破綻ではなく劣化)。
    #[test]
    fn engine_slots_are_capped_per_track() {
        let mut schedule: Vec<RenderedEvent> = (0..MAX_STRETCH_ENGINES_PER_TRACK + 3)
            .map(|i| test_event(StretchMode::Stretch, 0.0, i as f64 * 0.01, 100.0))
            .collect();
        let per_track = assign_engine_slots(&mut schedule);
        assert_eq!(per_track, vec![MAX_STRETCH_ENGINES_PER_TRACK as u16]);
        assert!(schedule[MAX_STRETCH_ENGINES_PER_TRACK].engine_slot.is_none());
    }

    /// フォルマントは **時間軸に効かない**: tape mode で値を入れても鳴る長さは
    /// 変わらない (= 波形描画 `audible_source_span` との一致条件)。 かつ出力は
    /// 実際に変化する (= 配線されている)。
    #[test]
    fn formant_shift_changes_timbre_without_changing_length() {
        let plain = render_clip_full(
            sine_source(48_000, 440.0),
            StretchMode::Raw,
            Vec::new(),
            0.0,
            0.0,
        );
        let shifted = render_clip_full(
            sine_source(48_000, 440.0),
            StretchMode::Raw,
            Vec::new(),
            0.0,
            12.0,
        );
        let ratio = last_audible(&shifted) as f64 / last_audible(&plain) as f64;
        // フォルマント段は STFT なので、素材の末尾が解析窓 (120 ms) のぶんだけ
        // 尾を引く。 「時間軸に効かない」 の検証としては、伸縮 (0.5x / 2x) と
        // 区別できる精度があれば十分なので窓 1 枚ぶんの余裕を持たせる。
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "フォルマントは時間軸に効かない (長さ不変) はず、 got {ratio}"
        );
        let mid_a = &plain[plain.len() / 3..plain.len() * 2 / 3];
        let mid_b = &shifted[shifted.len() / 3..shifted.len() * 2 / 3];
        let diff: f64 = mid_a
            .iter()
            .zip(mid_b.iter())
            .map(|(a, b)| f64::from((a - b).abs()))
            .sum::<f64>()
            / mid_a.len() as f64;
        assert!(
            diff > 1e-3,
            "tape mode でもフォルマント指定が出力に効くべき (配線確認)、 差 {diff}"
        );
        // 音程は動かない (倍音格子は 440 Hz のまま)。
        let m440 = magnitude_at(mid_b, 440.0);
        let m880 = magnitude_at(mid_b, 880.0);
        assert!(
            m440 > m880 * 4.0,
            "フォルマントを動かしても音程は不動: 440={m440} 880={m880}"
        );
    }

    /// r.md #40 の RT 不変条件の機械検査: スペクトル経路 (prime を含む) が
    /// audio thread で **確保も解放もしない**。`rt-assert` の allocator hook が
    /// 要る (`cargo test -p daw_audio --features rt-assert`)。
    ///
    /// C++ 側は `sms_create` の noise warm-up で内部 `std::vector` の高水位を
    /// off-RT に追い出してあり、Rust 側 wrapper も scratch を `new` で確保済。
    /// ここが落ちたらどちらかの前提が崩れている。
    #[cfg(feature = "rt-assert")]
    #[test]
    fn spectral_render_does_not_allocate_on_the_audio_thread() {
        let buffer = ramped_sine_source(48_000);
        let source_frames = buffer.frames;
        let mut schedule = vec![RenderedEvent {
            track_idx: 0,
            clip_idx: 0,
            start_beat: 0.0,
            end_beat: LEN_BEATS,
            source_id: 1,
            source_start_frames: 0,
            source_end_frames: source_frames,
            gain_lin: 1.0,
            pan: 0.0,
            sr_ratio: 1.0,
            pitch_factor: 1.0,
            pitch_semitones: 5.0,
            formant_semitones: -3.0,
            stream_key: 1,
            engine_slot: None,
            stretch_ratio: stretch_ratio_for(source_frames, 48_000, LEN_BEATS, BPM),
            nominal_bpm: BPM,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: false,
            stretch_mode: StretchMode::Stretch,
            onsets: Vec::new(),
            beat_markers: Vec::new(),
        }];
        let engines_per_track = assign_engine_slots(&mut schedule);
        let mut sources = HashMap::new();
        sources.insert(1u32, Arc::new(buffer));
        let renderer = AudioClipRenderer {
            schedule,
            sources,
            engines_per_track,
        };

        // エンジンと scratch は off-RT で用意する (= live では publish 側が作って
        // ring で配送、export では walk の頭で積む)。
        let mut engines = vec![StretchEngine::new(48_000).expect("engine")];
        let mut accum = vec![(u64::MAX, 0.0f64); 4];
        let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut l = vec![0.0f32; 512];
        let mut r = vec![0.0f32; 512];
        let samples_per_beat = f64::from(ENGINE_SR) * 60.0 / f64::from(BPM);

        // 1 回目は prime (= `sms_output_seek`) を含む発音開始、2 回目以降は定常。
        // どちらも RT で走るのでまとめて検査する。
        assert_no_alloc::assert_no_alloc(|| {
            for buf in 0..4u64 {
                render_audio_events(
                    &renderer,
                    0,
                    &mut l,
                    &mut r,
                    (buf * 512) as f64 / samples_per_beat,
                    BPM,
                    ENGINE_SR,
                    512,
                    &mut ClipRenderState {
                        repitch_accum: &mut accum,
                        engines: &mut engines,
                        event_l: &mut event_l,
                        event_r: &mut event_r,
                    },
                );
            }
        });
    }

    // ---- tape 位置積分 (E5 / r.md #8) --------------------------------------

    #[test]
    fn repitch_integrates_position_continuously_across_tempo_change() {
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
