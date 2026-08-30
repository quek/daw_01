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
    /// **このバッファの同一性**: decode 元の解決済み絶対パス。
    ///
    /// `AudioClipRenderer::sources` の key である `AudioSourceId` は
    /// **Song スコープの名前**でしかない (`IdAllocators::next_audio_source_id`
    /// は project ごとに 1 から再採番される)。id 一致だけで前 renderer の
    /// バッファを再利用すると、別 project を開いたとき **前の project の音源が
    /// 鳴る**。再利用の可否はこの origin の一致で判定する (= キャッシュの
    /// 同一性はキャッシュされた値自身が持つ)。
    pub origin: std::path::PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

impl AudioSourceBuffer {
    /// Empty silent buffer — used as a placeholder when the source is
    /// missing or still decoding. Allocates `frames` zeros per channel.
    /// `origin` は placeholder が「どの file の代わりか」 (= 再利用判定の
    /// 同一性)。
    pub fn silent(
        origin: std::path::PathBuf,
        sample_rate: u32,
        channels: u16,
        frames: u64,
    ) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            origin,
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
    /// r.md #87: この event を出したのが **ランチャーのセル**なら、そのセルの
    /// `clip.id`。アレンジのクリップは `0`。`render_audio_events` は行の供給元に
    /// 応じてこの値で選り分ける (旧 `clip_idx` は `track.clips` の positional index で、
    /// 書かれるだけで一度も読まれていなかった = アーキ不変条件 1 の負債)。
    pub cell_clip_id: u32,
    /// First song-beat this event contributes audio at。
    ///
    /// **event 自身の時間写像の起点** (source 読み出し / fade はここが 0 点)。
    /// clip の窓による crop は [`gate_start_beat`](Self::gate_start_beat) 側で行い、
    /// この値は動かさない (動かすと warp / slice / spectral の写像がずれる)。
    pub start_beat: f64,
    /// Exclusive end song-beat。
    pub end_beat: f64,
    /// r.md #44: この event を含む clip の窓 (song-absolute 拍) の開始。
    /// `[gate_start_beat, gate_end_beat)` の外は 1 sample も出力しない。
    /// linked clip は content を共有するので、鳴る範囲を決めるのは content ではなく
    /// **この窓** (= clip ごとに独立した開始・終了)。
    pub gate_start_beat: f64,
    /// clip の窓の終端 (exclusive、song-absolute 拍)。
    pub gate_end_beat: f64,
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
    /// この event はスペクトルエンジンを要るか (compile 時に確定)。
    /// **どのエンジンを使うかは持たない** — pool は `stream_key` で引く
    /// (位置インデックスを持たせると、無関係な clip の追加/削除で貪欲彩色の
    /// 色が玉突きして発音中の clip が別エンジンに移り、`sms_output_seek` の
    /// 内部 `reset` = OLA テール破棄でプツッと鳴る。 アーキ不変条件 #1)。
    pub needs_engine: bool,
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
        origin: path.to_path_buf(),
        sample_rate: decoded.sample_rate,
        channels: decoded.channels,
        frames: decoded.frames,
        samples: decoded.samples,
    })
}

/// `AudioSource` が実際にどのファイルを指すかを解決する (= decode 元、かつ
/// キャッシュ再利用の同一性)。`ProjectRelative` は `project_dir` 基準、
/// `Absolute` はそのまま。`Generated` はファイルを持たない (PR-V4 で廃止、
/// decode 対象外) ので `None`。
///
/// `compile_audio_schedule` と [`has_undecoded_sources`] が **同じ判定**を
/// 使うための単一定義 — ここがズレると「再利用もされず decode も予約されない
/// 源」が生まれて恒久的に無音になる。
pub fn resolve_source_path(
    source: &common::model::AudioSource,
    project_dir: Option<&Path>,
) -> Option<std::path::PathBuf> {
    match &source.path {
        AudioSourcePath::Absolute(abs) => Some(abs.clone()),
        AudioSourcePath::ProjectRelative(rel) => project_dir.map(|dir| dir.join(rel)),
        AudioSourcePath::Generated { .. } => None,
    }
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
        let Some(abs) = resolve_source_path(source, project_dir) else {
            // 解決できないのは 2 通りだけ: (a) `project_dir` 未設定の
            // `ProjectRelative`、(b) `Generated` (PR-V4 で廃止 — VOICEVOX は
            // builtin plugin 経由)。どちらも decode 対象外なので skip する。
            // IPC 受信ループ上なので `unreachable!` 等で panic させない
            // (audio プロセスごと落ちる)。
            tracing::warn!(
                source_id = id,
                path = ?source.path,
                "audio source path unresolved (project_dir unset or Generated); skipping"
            );
            continue;
        };
        // (A) reuse an already-decoded buffer from the live renderer — ただし
        //     **同じファイルを decode したものに限る** (r.md #7 decode 再設計 A)。
        //     `AudioSourceId` は Song スコープの名前で、project ごとに 1 から
        //     再採番される。id 一致だけで再利用すると、別 project を開いたときに
        //     前 project の音源が id 衝突でそのまま鳴る (しかも
        //     `has_undecoded_sources` が false になり decode も走らない)。
        //     同一 project 内の再 compile (BPM 変更 / 編集 / scrub) では origin が
        //     一致するので、従来どおり decode ゼロで済む。
        if let Some(prev) = prev
            && let Some(buf) = prev.sources.get(&id)
            && buf.origin == abs
        {
            sources.insert(id, Arc::clone(buf));
            continue;
        }
        // (B) not cached: decode now only if asked. Otherwise leave it out — its
        //     events drop from the schedule until a later full compile fills it.
        if !decode_missing {
            continue;
        }
        match decode_audio(&abs) {
            Ok(buf) => {
                sources.insert(id, Arc::new(buf));
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %abs.display(), "decode failed");
            }
        }
    }

    // -- Flatten every audio clip's events into RenderedEvent ----------------
    let mut schedule: Vec<RenderedEvent> = Vec::new();
    for (track_idx, track) in song.tracks.iter().enumerate() {
        for clip in &track.clips {
            // 隣接クリップとのクロスフェード: **隣が実在するときだけ**窓を外へ緩める。
            // これで、クリップを動かして隣が居なくなったら `xfade_*` が残っていても
            // 無視され、stale な音漏れが起きない (`Clip::xfade_lead_beats` の doc)。
            // 隣接走査は **張り出しを要求しているクリップだけ** に絞る
            // (要求ゼロが普通なので、クリップ数の 2 乗走査を実質 O(N) にする)。
            let want_lead = clip.xfade_lead_beats > 0.0;
            let want_tail = clip.xfade_tail_beats > 0.0;
            let (s0, e0) = clip.song_window();
            let has_prev = want_lead
                && track.clips.iter().any(|o| {
                    o.id != clip.id && (o.start_beat + o.length_beats - s0).abs() < 1e-6
                });
            let has_next = want_tail
                && track
                    .clips
                    .iter()
                    .any(|o| o.id != clip.id && (o.start_beat - e0).abs() < 1e-6);
            let lead = if has_prev { clip.xfade_lead_beats.max(0.0) } else { 0.0 };
            let tail = if has_next { clip.xfade_tail_beats.max(0.0) } else { 0.0 };
            push_clip_events(
                &mut schedule,
                song,
                &sources,
                track_idx,
                clip,
                engine_sample_rate,
                ClipPlacement { cell_clip_id: 0, xfade: (lead, tail) },
            );
        }
        // r.md #87: ランチャーのセルも同じ schedule に載せる。`clip.start_beat` は
        // 0 なので窓は `[0, length)` = セル自身の座標になり、行の実効拍を渡すだけで
        // 鳴る。走査時は `cell_clip_id` で選り分けるので、アレンジ行には漏れない。
        for cell in &track.session_clips {
            let id = cell.clip.id;
            // セルは隣接という概念が無い (グリッドに時間軸が無い) ので張り出しゼロ。
            push_clip_events(
                &mut schedule,
                song,
                &sources,
                track_idx,
                &cell.clip,
                engine_sample_rate,
                ClipPlacement { cell_clip_id: id, xfade: (0.0, 0.0) },
            );
        }
    }
    schedule.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    let engines_per_track = count_engines_per_track(&mut schedule);
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

/// 1 クリップ (アレンジのクリップ or ランチャーのセル) の audio event を
/// `schedule` へ平坦化する。
///
/// r.md #87: **セルもアレンジのクリップと完全に同じ算術**で載る — 窓は
/// `clip.song_window()`、event の起点は `content_to_song_beat`。違うのは
/// `cell_clip_id` (0 = アレンジ、それ以外 = そのセルの `clip.id`) だけで、
/// `render_audio_events` が行の供給元に応じてこれで選り分ける。
///
/// `compile_audio_schedule` の二重ループから切り出したのは、そこへセル用の
/// ループを入れ子で足すと `scripts/arch_lint_baseline.txt` の FN-NESTING 天井
/// (`compile_audio_schedule 7/5`) を即座に超えるため (不変条件 9)。
/// 1 クリップを schedule へ載せるときの「置かれ方」。
///
/// `cell_clip_id` は 0 = アレンジのクリップ、それ以外 = そのランチャーセルの `clip.id`。
/// `xfade` は隣接クリップとのクロスフェードで窓の外へ鳴らし進める量 `(先頭側, 末尾側)` (拍)
/// で、**隣が実在するときだけ**非ゼロで渡ってくる。
#[derive(Clone, Copy)]
struct ClipPlacement {
    cell_clip_id: u32,
    xfade: (f64, f64),
}

fn push_clip_events(
    schedule: &mut Vec<RenderedEvent>,
    song: &Song,
    sources: &HashMap<AudioSourceId, Arc<AudioSourceBuffer>>,
    track_idx: usize,
    clip: &common::model::Clip,
    engine_sample_rate: u32,
    placement: ClipPlacement,
) {
    let ClipPlacement { cell_clip_id, xfade } = placement;
    let Some(content) = song.clip_contents.get(&clip.content_id) else {
        return;
    };
    let ClipContent::Audio(audio) = content else {
        return;
    };
    // muted clip は全 audio event を schedule から除外する
    // (per-event `event.muted` とは独立。clip-level mute の SSoT)。
    if clip.muted {
        return;
    }
    // r.md #44: clip は content への窓。 窓 (`[start_beat, +length_beats)`) の
    // 外にはみ出す event 部分は鳴らさない。 event 自身の時間写像
    // (`start_beat` 起点の source 読み出し / fade) は **一切動かさず**、
    // 出力範囲だけを交差させる (source 窓を切り詰めると warp marker /
    // slice onset / spectral stretch の写像が壊れるため)。
    let (clip_gate_start, clip_gate_end) = {
        let (s, e) = clip.song_window();
        // クリップ同士は重ならないので、境界で音を途切れさせないには **鳴らす範囲だけ**
        // を隣の側へ伸ばす (`docs/plan_range_selection.md` §6.5)。 伸ばした区間は
        // event の fade ランプが覆うので、境界を挟んで左が下がり右が上がる。
        (s - xfade.0, e + xfade.1)
    };
    for (event_seq, event) in audio.events.iter().enumerate() {
        let Some(buffer) = sources.get(&event.source_id) else {
            continue;
        };
        let event_start_beat =
            clip.content_to_song_beat(event.event_start_in_clip_beats);
        let event_end_beat = event_start_beat + event.event_length_beats;
        if event_end_beat <= event_start_beat {
            continue;
        }
        // 窓と交差しない event は schedule に載せない。
        if event_end_beat <= clip_gate_start || event_start_beat >= clip_gate_end {
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
            cell_clip_id,
            start_beat: event_start_beat,
            end_beat: event_end_beat,
            gate_start_beat: clip_gate_start,
            gate_end_beat: clip_gate_end,
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
            // 判定は schedule 完成後 (`count_engines_per_track`)。
            needs_engine: false,
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

/// 各 event の `needs_engine` を確定し、track index → **必要エンジン数**
/// (= 同時に鳴りうる Stretch / formant≠0 event の最大数) を返す。
/// off-RT (compile 時) 専用。
///
/// 数だけを出し、**どの event がどのエンジンを使うかは決めない**。 区間グラフの
/// 貪欲彩色は「使う色数 = 最大同時発音数」で最適だが、**event 集合が変わると
/// 既存 event の色も動く** (例: A[0,2)→0, B[4,8)→0 の間に C[3,5) を足すと B が
/// slot1 へ玉突きする)。 色を永続 pool の位置として使うと、その瞬間に発音中の
/// clip が別エンジンへ移って再 prime = 内部 `reset` (上流ヘッダ自身が
/// 「full reset はクリックが出る」 と TODO に明記) になる。 引き当ては RT 側で
/// `stream_key` で行い、ここは容量計画だけを担う。
fn count_engines_per_track(schedule: &mut [RenderedEvent]) -> Vec<u16> {
    // track index → 「現在鳴っている event の end_beat」 の集合 (同時発音数の計測)。
    let mut per_track: HashMap<usize, Vec<f64>> = HashMap::new();
    let mut max_track = 0usize;
    for ev in schedule.iter_mut() {
        max_track = max_track.max(ev.track_idx);
        ev.needs_engine = needs_stretch_engine(ev);
        if !ev.needs_engine {
            continue;
        }
        let live = per_track.entry(ev.track_idx).or_default();
        // r.md #44: 同時発音の判定は **実際に鳴る区間** (= event span ∩ clip の窓)。
        // 窓で切られて短くなった event に素の span を使うと過大に数えてしまう。
        let audible_start = ev.start_beat.max(ev.gate_start_beat);
        let audible_end = ev.end_beat.min(ev.gate_end_beat);
        // 既に終わっている枠を再利用 (= 同時発音数を数える貪欲彩色と同型)。
        match live.iter().position(|&end| end <= audible_start) {
            Some(i) => live[i] = audible_end,
            None if live.len() < MAX_STRETCH_ENGINES_PER_TRACK => live.push(audible_end),
            // 上限超過: RT で引き当てに失敗し、エンジン無しで degrade する。
            None => {}
        }
    }
    let mut per_track_counts = vec![0u16; if schedule.is_empty() { 0 } else { max_track + 1 }];
    for (track, live) in per_track {
        if let Some(slot) = per_track_counts.get_mut(track) {
            *slot = u16::try_from(live.len()).unwrap_or(u16::MAX);
        }
    }
    per_track_counts
}

/// pool から `key` の発音に対応するエンジンを **安定キーで**引き当てる (RT)。
///
/// 1. 既にその発音を走らせているエンジン (= 継続。無関係な編集で pool の並びが
///    変わっても同じ実体に戻るので、発音中に prime し直さない)
/// 2. 空きエンジン (まだどの発音も持っていない)
/// 3. この buffer で誰にも取られていないエンジン (= 既に鳴り終わった発音の
///    使い回し。取られたら旧発音側は次に鳴るとき prime し直す)
///
/// `stamp` は buffer ごとの連番で、同じ buffer 内で 2 つの発音が同じエンジンを
/// 掴むのを防ぐ。 どれも取れなければ `None` (= degrade)。
/// RT-safe: 線形探索のみ (pool は最大 32)。
fn acquire_engine(
    engines: &mut [StretchEngine],
    key: u64,
    stamp: u64,
) -> Option<&mut StretchEngine> {
    let pick = engines
        .iter()
        .position(|e| e.stream_key() == Some(key) && !e.is_claimed(stamp))
        .or_else(|| {
            engines
                .iter()
                .position(|e| e.stream_key().is_none() && !e.is_claimed(stamp))
        })
        .or_else(|| engines.iter().position(|e| !e.is_claimed(stamp)))?;
    let engine = engines.get_mut(pick)?;
    engine.claim(stamp);
    Some(engine)
}

/// Does `song` reference any file-backed `AudioSource` that `renderer` has not
/// decoded yet? `true` ⇒ the background decode worker must run a full compile to
/// fill them in. `Generated` sources are excluded (never decoded here).
///
/// 「decode 済み」の判定は `compile_audio_schedule` の再利用条件と**同一**
/// (id 一致 **かつ** origin 一致) でなければならない。id だけで見ると、別
/// project の同 id バッファが「decode 済み」に見えて decode が予約されず、
/// 前 project の音が鳴り続ける。
pub fn has_undecoded_sources(
    song: &Song,
    renderer: &AudioClipRenderer,
    project_dir: Option<&Path>,
) -> bool {
    song.media.audio_sources.iter().any(|(id, source)| {
        let Some(abs) = resolve_source_path(source, project_dir) else {
            // Generated (廃止) / project_dir 未設定の ProjectRelative は
            // そもそも decode 対象外なので「未 decode」に数えない。
            return false;
        };
        renderer
            .sources
            .get(id)
            .is_none_or(|buf| buf.origin != abs)
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
    /// buffer ごとに増える連番。 同じ buffer 内で 2 つの発音が同じエンジンを
    /// 掴むのを防ぐ (`acquire_engine`)。 実体は `TrackScratch` にある。
    pub render_seq: &'a mut u64,
}

#[allow(clippy::too_many_arguments)]
pub fn render_audio_events(
    renderer: &AudioClipRenderer,
    track_idx: usize,
    // r.md #87: この行が今どこから鳴っているか。`0` = アレンジのクリップ列、
    // それ以外 = そのセル 1 つ (`RenderedEvent::cell_clip_id` と一致する event だけ描く)。
    cell_clip_id: u32,
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
    // pool 争奪の解決用 (同 buffer 内で 2 発音が同じエンジンを掴まないように)。
    *state.render_seq = state.render_seq.wrapping_add(1);
    let render_seq = *state.render_seq;
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
        // r.md #87: 行の供給元と違う出どころの event は描かない。判定は
        // `accum_idx` を進めた **後**なので、アレンジ ↔ ランチャーを行き来しても
        // `repitch_accum` の添字は同じ event に張り付いたままになる。
        if event.cell_clip_id != cell_clip_id {
            continue;
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
        //
        // r.md #44: さらに **clip の窓** (`gate_*`) とも交差させる。 event の
        // 時間写像 (`event.start_beat` 起点) は動かさないので、窓の内側だけが
        // 鳴り、窓の外は無音になる (= clip の開始・終了が linked clip ごとに独立)。
        let render_start_beat = event
            .start_beat
            .max(playhead_beats)
            .max(event.gate_start_beat);
        let render_end_beat = event.end_beat.min(buf_end_beats).min(event.gate_end_beat);
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
        // `common::audio_render::event_wave_spans` と同じ写像でなければならない
        // (= 描いた波形と鳴る音が一致する条件)。 片方だけ変えると
        // 下の `wave_span_binding_tests` (実レンダリング出力と span 列の突き合わせ)
        // が落ちる。
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
        let engine = if event.needs_engine {
            acquire_engine(state.engines, event.stream_key, render_seq)
        } else {
            None
        };

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
            origin: std::path::PathBuf::from("test://ramp"),
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
            origin: std::path::PathBuf::from("test://ramped-sine"),
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
            origin: std::path::PathBuf::from("test://sine"),
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

    fn render_clip_full(
        buffer: AudioSourceBuffer,
        mode: StretchMode,
        onsets: Vec<u64>,
        semitones: f32,
        formant: f32,
    ) -> Vec<f32> {
        render_clip_stereo(buffer, mode, onsets, semitones, formant).0
    }

    /// clip 全長を 512 frame ずつ render して **L / R 両方**を返す。
    /// engine pool は `count_engines_per_track` が出した必要数を off-RT で確保する
    /// (= live の publish 経路 / export の walk と同じ手順)。
    fn render_clip_stereo(
        buffer: AudioSourceBuffer,
        mode: StretchMode,
        onsets: Vec<u64>,
        semitones: f32,
        formant: f32,
    ) -> (Vec<f32>, Vec<f32>) {
        let source_sr = buffer.sample_rate;
        let source_frames = buffer.frames;
        let mut schedule = vec![RenderedEvent {
            track_idx: 0,
            cell_clip_id: 0,
            start_beat: 0.0,
            end_beat: LEN_BEATS,
            gate_start_beat: f64::NEG_INFINITY,
            gate_end_beat: f64::INFINITY,
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
            needs_engine: false,
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
        let engines_per_track = count_engines_per_track(&mut schedule);
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
        let mut render_seq = 0u64;
        let mut out_l: Vec<f32> = Vec::with_capacity(total);
        let mut out_r: Vec<f32> = Vec::with_capacity(total);
        while out_l.len() < total {
            let frames = 512.min(total - out_l.len());
            let mut l = vec![0.0f32; frames];
            let mut r = vec![0.0f32; frames];
            render_audio_events(
                &renderer,
                0,
                0,
                &mut l,
                &mut r,
                out_l.len() as f64 / samples_per_beat,
                BPM,
                ENGINE_SR,
                frames as u32,
                &mut ClipRenderState {
                    repitch_accum: &mut accum,
                    engines: &mut engines,
                    event_l: &mut event_l,
                    event_r: &mut event_r,
                    render_seq: &mut render_seq,
                },
            );
            out_l.extend_from_slice(&l);
            out_r.extend_from_slice(&r);
        }
        // pan 中央 (equal-power × √2) は利得 1.0 に戻る前提を固定する。
        (out_l, out_r)
    }

    /// r.md #44: clip の窓 (`gate_*`) の外は 1 sample も出さない。 event 自身の
    /// 時間写像 (`start_beat` 起点の source 読み出し) は変えないので、窓の内側は
    /// gate 無しのときと **完全に同じ波形** になる。
    ///
    /// これが audio 側の「clip の開始・終了」 の実体で、旧実装は gate を持たず
    /// event 長だけが鳴る範囲を決めていた (= 共有 content を書き換えないと clip 長が
    /// 効かず、書き換えると linked clip を巻き込む、という袋小路だった)。
    #[test]
    fn clip_window_gate_mutes_outside_and_keeps_inside_identical() {
        let source = Arc::new(ramped_sine_source(ENGINE_SR));
        let source_frames = source.frames;
        let samples_per_beat = f64::from(ENGINE_SR) * 60.0 / f64::from(BPM);
        let render = |gate: (f64, f64)| -> Vec<f32> {
            let mut schedule = vec![RenderedEvent {
                track_idx: 0,
                cell_clip_id: 0,
                start_beat: 0.0,
                end_beat: LEN_BEATS,
                gate_start_beat: gate.0,
                gate_end_beat: gate.1,
                source_id: 1,
                source_start_frames: 0,
                source_end_frames: source_frames,
                gain_lin: 1.0,
                pan: 0.0,
                sr_ratio: sample_rate_ratio(source.sample_rate, ENGINE_SR),
                pitch_factor: 1.0,
                pitch_semitones: 0.0,
                formant_semitones: 0.0,
                stream_key: 1,
                needs_engine: false,
                stretch_ratio: 1.0,
                nominal_bpm: BPM,
                fade_in_beats: 0.0,
                fade_out_beats: 0.0,
                fade_in_curve: FadeCurve::Linear,
                fade_out_curve: FadeCurve::Linear,
                reversed: false,
                stretch_mode: StretchMode::Raw,
                onsets: Vec::new(),
                beat_markers: Vec::new(),
            }];
            let engines_per_track = count_engines_per_track(&mut schedule);
            let mut sources = HashMap::new();
            sources.insert(1u32, Arc::clone(&source));
            let renderer = AudioClipRenderer { schedule, sources, engines_per_track };
            let total = (LEN_BEATS * samples_per_beat) as usize;
            let mut accum = vec![(u64::MAX, 0.0f64); 4];
            let mut engines: Vec<StretchEngine> = Vec::new();
            let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
            let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
            let mut render_seq = 0u64;
            let mut out: Vec<f32> = Vec::with_capacity(total);
            while out.len() < total {
                let frames = 512.min(total - out.len());
                let mut l = vec![0.0f32; frames];
                let mut r = vec![0.0f32; frames];
                render_audio_events(
                    &renderer,
                    0,
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
                        render_seq: &mut render_seq,
                    },
                );
                out.extend_from_slice(&l);
            }
            out
        };

        let full = render((f64::NEG_INFINITY, f64::INFINITY));
        // 窓を [1 拍, 2 拍) に狭める (= clip を左右から trim した状態)。
        let gated = render((1.0, 2.0));
        let one = samples_per_beat as usize;
        assert!(
            gated[..one].iter().all(|s| *s == 0.0),
            "窓より前は無音でなければならない"
        );
        assert!(
            gated[2 * one..].iter().all(|s| *s == 0.0),
            "窓より後は無音でなければならない"
        );
        assert_eq!(
            &gated[one..2 * one],
            &full[one..2 * one],
            "窓の内側は gate 無しと 1 sample も違わない (= source 窓を動かしていない証拠)"
        );
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

    /// 素材の第 1 フォルマント (= 包絡の山) の位置。 `f0` = 200 / 400 の
    /// どちらの倍音格子にも乗る値なので、移調前後で同じ指標が使える。
    const VOWEL_F1_HZ: f64 = 800.0;

    /// 共振ピーク 2 本 (`VOWEL_F1_HZ` / 1600 Hz) を持つ合成母音。 倍音の**位置**が
    /// 音程、倍音の**振幅の山**が声質 (フォルマント) で、この 2 つが独立に動くかを
    /// render 経路で測るための素材。
    fn vowel_source(source_sr: u32, f0: f64) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let env = |f: f64| -> f64 {
            1.0 / (1.0 + ((f - VOWEL_F1_HZ) / 110.0).powi(2))
                + 0.45 / (1.0 + ((f - 1600.0) / 160.0).powi(2))
                + 0.02
        };
        let harmonics: Vec<(f64, f64)> = (1..=30)
            .map(|k| (f0 * f64::from(k), env(f0 * f64::from(k))))
            .filter(|(f, _)| *f < f64::from(source_sr) / 2.0 * 0.8)
            .collect();
        let norm: f64 = harmonics.iter().map(|(_, a)| a).sum::<f64>().max(1e-9);
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(source_sr);
                let v: f64 = harmonics
                    .iter()
                    .map(|(f, a)| a * (std::f64::consts::TAU * f * t).sin())
                    .sum();
                (v / norm * 0.8) as f32
            })
            .collect();
        AudioSourceBuffer {
            origin: std::path::PathBuf::from("test://vowel"),
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    /// `f0` の倍音のうち最も強いものの周波数 = スペクトル包絡の山の位置。
    fn envelope_peak_hz(x: &[f32], f0: f64) -> f64 {
        let mut best = (0.0f64, 0.0f64);
        let mut k = 1u32;
        while (f0 * f64::from(k)) < 4000.0 {
            let f = f0 * f64::from(k);
            let m = magnitude_at(x, f);
            if m > best.1 {
                best = (f, m);
            }
            k += 1;
        }
        best.0
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
            cell_clip_id: 0,
            start_beat: start,
            end_beat: end,
            gate_start_beat: f64::NEG_INFINITY,
            gate_end_beat: f64::INFINITY,
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
            needs_engine: false,
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
            let per_track = count_engines_per_track(&mut schedule);
            assert_eq!(schedule[0].needs_engine, want, "{mode:?} formant={formant}");
            assert_eq!(per_track.first().copied().unwrap_or(0), u16::from(want));
        }
    }

    /// pool の必要数は **最大同時発音数**であって event 数ではない。
    /// これを誤ると track あたり数百 MB のエンジンを確保して破綻する。
    #[test]
    fn engine_count_is_max_concurrency_not_event_count() {
        let mut schedule = vec![
            test_event(StretchMode::Stretch, 0.0, 0.0, 4.0),
            // 重なる → 同時に 2 個要る
            test_event(StretchMode::Stretch, 0.0, 2.0, 6.0),
            // 上 2 つが終わってから → 追加は要らない
            test_event(StretchMode::Stretch, 0.0, 8.0, 12.0),
        ];
        let per_track = count_engines_per_track(&mut schedule);
        assert!(schedule.iter().all(|e| e.needs_engine));
        assert_eq!(per_track, vec![2], "同時発音数 = 2 個で足りる");
    }

    /// 必要数は上限で頭打ちになる (溢れた発音は RT の引き当て失敗で degrade)。
    #[test]
    fn engine_count_is_capped_per_track() {
        let mut schedule: Vec<RenderedEvent> = (0..MAX_STRETCH_ENGINES_PER_TRACK + 3)
            .map(|i| test_event(StretchMode::Stretch, 0.0, i as f64 * 0.01, 100.0))
            .collect();
        let per_track = count_engines_per_track(&mut schedule);
        assert_eq!(per_track, vec![MAX_STRETCH_ENGINES_PER_TRACK as u16]);
    }

    /// **アーキ不変条件 #1**: エンジンの引き当ては位置ではなく安定キー。
    /// 無関係な clip の追加で pool 内の並びが変わっても、発音中の clip は
    /// 同じエンジン実体に戻る (= 発音中に prime し直さない)。
    #[test]
    fn engine_is_reacquired_by_stable_key_not_position() {
        let mut engines: Vec<StretchEngine> = (0..3)
            .map(|_| StretchEngine::new(ENGINE_SR).expect("stretch engine"))
            .collect();
        // 3 発音がそれぞれ別エンジンを掴む。
        for (i, key) in [10u64, 20, 30].iter().enumerate() {
            let e = acquire_engine(&mut engines, *key, 1).expect("engine");
            // 走行中ストリームを作るため 1 サンプルだけ回す。
            let mut l = [0.0f32; 1];
            let mut r = [0.0f32; 1];
            e.render(*key, 0.0, 0.0, false, 0, 1.0, |el| el as f64, |_| (0.0, 0.0), &mut l, &mut r);
            assert_eq!(e.stream_key(), Some(*key), "{i} 番目");
        }
        // key 20 のエンジンの実体アドレスを控える。
        let addr20 = engines
            .iter()
            .position(|e| e.stream_key() == Some(20))
            .expect("key 20 のエンジン");
        // 次の buffer で順番を入れ替えて引き当てても、同じ実体に戻る。
        for key in [30u64, 20, 10] {
            let picked = engines
                .iter()
                .position(|e| e.stream_key() == Some(key))
                .expect("key に対応するエンジン");
            let e = acquire_engine(&mut engines, key, 2).expect("engine");
            assert_eq!(e.stream_key(), Some(key));
            if key == 20 {
                assert_eq!(picked, addr20, "key 20 は同じエンジン実体に戻る");
            }
        }
    }

    /// pool が足りないときは degrade する (= 他の発音のエンジンを奪って
    /// 無限に prime し合わない)。 同じ buffer 内で二重取りしないことも固定。
    #[test]
    fn engine_acquire_degrades_when_pool_is_exhausted() {
        let mut engines: Vec<StretchEngine> =
            vec![StretchEngine::new(ENGINE_SR).expect("stretch engine")];
        assert!(acquire_engine(&mut engines, 1, 7).is_some());
        assert!(
            acquire_engine(&mut engines, 2, 7).is_none(),
            "同じ buffer で 2 発音が同じエンジンを掴んではいけない"
        );
        // buffer が変われば使い回せる。
        assert!(acquire_engine(&mut engines, 2, 8).is_some());
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
        // formant 0 の tape はエンジンを通らない per-sample 経路、formant≠0 は
        // STFT 経路なので、この 2 つの差は「STFT を通ったか」しか測れない
        // (= 恒真に近い)。 formant 値そのものが届いているかは、**同じ STFT 経路
        // どうし** (+12 vs -12) を比べて初めて分かる。
        let up = render_clip_full(
            sine_source(48_000, 440.0),
            StretchMode::Raw,
            Vec::new(),
            0.0,
            12.0,
        );
        let down = render_clip_full(
            sine_source(48_000, 440.0),
            StretchMode::Raw,
            Vec::new(),
            0.0,
            -12.0,
        );
        let mid_up = &up[up.len() / 3..up.len() * 2 / 3];
        let mid_down = &down[down.len() / 3..down.len() * 2 / 3];
        let diff: f64 = mid_up
            .iter()
            .zip(mid_down.iter())
            .map(|(a, b)| f64::from((a - b).abs()))
            .sum::<f64>()
            / mid_up.len() as f64;
        assert!(
            diff > 1e-3,
            "tape mode でも formant_semitones の値が engine に届くべき (配線確認)、 差 {diff}"
        );
        // 音程は動かない (倍音格子は 440 Hz のまま)。
        let m440 = magnitude_at(mid_up, 440.0);
        let m880 = magnitude_at(mid_up, 880.0);
        assert!(
            m440 > m880 * 4.0,
            "フォルマントを動かしても音程は不動: 440={m440} 880={m880}"
        );
    }

    /// **r.md #40 の主契約を render 経路で固定する**: Stretch でピッチを上げても
    /// スペクトル包絡 (= 声質) の山が動かない。
    ///
    /// エンジン層のテスト (`crate::stretch_engine`) は `compensate` を
    /// **テスト自身が渡す**ので、`render_audio_events` 側の配線
    /// (Stretch は `compensate_pitch = true`) が壊れても素通しになる。
    /// ここは依頼文そのものを render 経路で押さえる。
    #[test]
    fn stretch_pitch_preserves_the_formant_through_the_render_path() {
        let dry = render_clip_full(
            vowel_source(48_000, 200.0),
            StretchMode::Stretch,
            Vec::new(),
            0.0,
            0.0,
        );
        let up = render_clip_full(
            vowel_source(48_000, 200.0),
            StretchMode::Stretch,
            Vec::new(),
            12.0,
            0.0,
        );
        // 過渡を避けて中央付近で測る。
        let dry_mid = &dry[dry.len() / 3..dry.len() * 2 / 3];
        let up_mid = &up[up.len() / 3..up.len() * 2 / 3];

        // 音程は 1 オクターブ上がる (F0 200 → 400)。
        let m200 = magnitude_at(up_mid, 200.0);
        let m400 = magnitude_at(up_mid, 400.0);
        assert!(
            m400 > m200 * 4.0,
            "Stretch +12 半音で F0 は 400 Hz になるべき: 400={m400} 200={m200}"
        );

        // 声質 (包絡の山) は動かない。 配線が `compensate_pitch = false` に
        // 倒れると山も 1 オクターブ上がるので、この assert が落ちる。
        let dry_peak = envelope_peak_hz(dry_mid, 200.0);
        let up_peak = envelope_peak_hz(up_mid, 400.0);
        assert!(
            (dry_peak - VOWEL_F1_HZ).abs() <= 200.0,
            "素材の包絡の山は {VOWEL_F1_HZ} Hz 付近のはず: {dry_peak}"
        );
        assert!(
            (up_peak - dry_peak).abs() <= 400.0,
            "移調しても包絡の山は動かないべき: 移調前 {dry_peak} Hz → 移調後 {up_peak} Hz"
        );
    }

    /// ステレオ経路 (`sms_process` の 2ch API、`event_l`/`event_r` scratch、
    /// pan 段) が L/R を取り違えていないこと。 全テストが mono 素材 + L のみ
    /// 収集だと、shim の `StereoOut{{out_l, out_l}}` のような取り違えを
    /// 1 件も検出できない。
    #[test]
    fn stereo_channels_are_kept_separate_through_the_spectral_path() {
        // L = 440 Hz、R = 660 Hz の別内容ステレオ素材。
        let frames = 48_000u64;
        let l: Vec<f32> = (0..frames)
            .map(|i| (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        let r: Vec<f32> = (0..frames)
            .map(|i| (std::f64::consts::TAU * 660.0 * i as f64 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        let buffer = AudioSourceBuffer {
            origin: std::path::PathBuf::from("test://stereo"),
            sample_rate: 48_000,
            channels: 2,
            frames,
            samples: vec![l, r],
        };
        let (out_l, out_r) =
            render_clip_stereo(buffer, StretchMode::Stretch, Vec::new(), 0.0, 0.0);

        let mid_l = &out_l[out_l.len() / 3..out_l.len() * 2 / 3];
        let mid_r = &out_r[out_r.len() / 3..out_r.len() * 2 / 3];
        let l440 = magnitude_at(mid_l, 440.0);
        let l660 = magnitude_at(mid_l, 660.0);
        let r440 = magnitude_at(mid_r, 440.0);
        let r660 = magnitude_at(mid_r, 660.0);
        assert!(
            l440 > l660 * 4.0,
            "L は 440 Hz のまま: 440={l440} 660={l660}"
        );
        assert!(
            r660 > r440 * 4.0,
            "R は 660 Hz のまま: 660={r660} 440={r440}"
        );
    }

    /// r.md #40 の RT 不変条件の機械検査: スペクトル経路 (prime を含む) が
    /// audio thread で **確保も解放もしない**。`make test-rt` で走る
    /// (`cargo test -p daw_audio --features rt-assert`)。
    ///
    /// **Rust 側と C++ 側を別々に検査する**:
    /// - Rust: `assert_no_alloc` の `#[global_allocator]` フック。
    /// - C++: `sms_alloc_count()` (= `alloc-count` feature が global
    ///   `operator new` を置換して数える)。 Rust の allocator フックは
    ///   **C++ の確保を一切見られない** (CRT へ直行するので不可視) ため、
    ///   これが無いと vendored エンジン側の realloc 回帰を緑のまま素通しする。
    ///
    /// 落ちたら: Rust 側なら wrapper に確保が入った、C++ 側なら
    /// `sms_create` の noise warm-up がカバーしない `resize` が増えた
    /// (vendor 更新 / preset・block・interval 変更) ということ。
    #[cfg(feature = "rt-assert")]
    #[test]
    fn spectral_render_does_not_allocate_on_the_audio_thread() {
        let buffer = ramped_sine_source(48_000);
        let source_frames = buffer.frames;
        let mut schedule = vec![RenderedEvent {
            track_idx: 0,
            cell_clip_id: 0,
            start_beat: 0.0,
            end_beat: LEN_BEATS,
            gate_start_beat: f64::NEG_INFINITY,
            gate_end_beat: f64::INFINITY,
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
            needs_engine: false,
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
        let engines_per_track = count_engines_per_track(&mut schedule);
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

        let mut render_seq = 0u64;

        // C++ 側の確保カウンタ。 計装されていない (= feature の配線ミス) なら
        // この検査は無意味なので、まずそれ自体を弾く。
        // SAFETY: 引数なしのカウンタ読み出し。
        let cxx_before = unsafe { signalsmith_sys::sms_alloc_count() };
        assert_ne!(
            cxx_before,
            u64::MAX,
            "signalsmith-sys/alloc-count が有効になっていない              (rt-assert から有効化されるはず) — C++ 側の確保を検査できない"
        );

        // 1 回目は prime (= `sms_output_seek`) を含む発音開始、2 回目以降は定常。
        // どちらも RT で走るのでまとめて検査する。
        assert_no_alloc::assert_no_alloc(|| {
            for buf in 0..4u64 {
                render_audio_events(
                    &renderer,
                    0,
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
                        render_seq: &mut render_seq,
                    },
                );
            }
        });

        // SAFETY: 同上。
        let cxx_after = unsafe { signalsmith_sys::sms_alloc_count() };
        assert_eq!(
            cxx_after, cxx_before,
            "vendored C++ エンジンが RT で {} 回ヒープ確保した              (warm-up がカバーしない resize が増えている)",
            cxx_after - cxx_before
        );
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

/// r.md #41: **波形描画の写像 (`common::audio_render::event_wave_spans`) と、
/// 実レンダリング出力が一致する**ことを直接 assert する束縛テスト。
///
/// 従来この 2 つはコメント (`render_audio_events` の写像節) でしか結び付いて
/// おらず、 片方だけ変えても CI が落ちなかった。 ここで
/// 「span が張られている拍区間は span の source 写像どおりに鳴る」
/// 「span が無い拍区間は完全に無音」 を検証し、 描画と再生の乖離を構造的に
/// 検出できるようにする。
///
/// 素材は経路で使い分ける (r.md #40 でスペクトル化されたため):
/// - tape / slice (`Raw` / `Repitch` / `Slice`): **ramp** (値 = 位置)。 sample 直読
///   なので出力値がそのまま source 位置。
/// - `Stretch` (spectral): **振幅 ramp のサイン波**。 位相ボコーダは位相を
///   ランダム化するので瞬時値は指標にならないが、 振幅エンベロープは解析位置に
///   追従するので局所 RMS で source 位置を測れる (`render_tests` と同じ手法)。
#[cfg(test)]
mod wave_span_binding_tests {
    use super::*;
    use common::audio_render::{TempoMap, WaveSpan, event_wave_spans};
    use common::model::AudioEvent;

    const ENGINE_SR: u32 = 48_000;
    const BPM: f32 = 120.0;
    /// 1 秒 (= 120 BPM で 2 拍) ぶんの素材。
    const SOURCE_FRAMES: u64 = 48_000;

    /// 値 = 位置 の ramp 素材 (tape / slice 経路用)。
    fn ramp(source_sr: u32) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames)
            .map(|i| i as f32 / (frames - 1) as f32)
            .collect();
        AudioSourceBuffer {
            origin: std::path::PathBuf::from("test://wave-span-ramp"),
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    /// 振幅エンベロープ = 位置 のサイン波素材 (spectral 経路用)。
    fn ramped_sine(source_sr: u32) -> AudioSourceBuffer {
        let frames = u64::from(source_sr);
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                let env = i as f32 / (frames - 1) as f32;
                let phase = std::f64::consts::TAU * 440.0 * i as f64 / f64::from(source_sr);
                (phase.sin() as f32) * env
            })
            .collect();
        AudioSourceBuffer {
            origin: std::path::PathBuf::from("test://wave-span-ramped-sine"),
            sample_rate: source_sr,
            channels: 1,
            frames,
            samples: vec![samples],
        }
    }

    fn local_rms(x: &[f32], center: usize, half_width: usize) -> f64 {
        let lo = center.saturating_sub(half_width);
        let hi = (center + half_width).min(x.len());
        if hi <= lo {
            return 0.0;
        }
        let sum: f64 = x[lo..hi].iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        (sum / (hi - lo) as f64).sqrt()
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

    /// render 結果 + buffer 境界ごとの (playhead 拍, 出力 sample index)。
    /// tempo 曲線下では拍 → sample が非線形なので、 この対応表で引く。
    struct Rendered {
        out: Vec<f32>,
        marks: Vec<(f64, usize)>,
    }

    impl Rendered {
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

    /// model の `AudioEvent` を `compile_audio_schedule` と **同じ写像** で
    /// `RenderedEvent` に落とし、 clip 全長を 512 frame ずつ render する。
    ///
    /// `tempo_song` を渡すと engine (`daw_audio::engine`) と同じく **buffer ごとに**
    /// `evaluate_song_tempo(song, playhead_beats)` で current_bpm を評価し、
    /// `playhead_beats += frames * bpm / (60*SR)` で進める。 `None` なら定数 `BPM`。
    fn render_model_event(
        event: &AudioEvent,
        buffer: AudioSourceBuffer,
        tempo_song: Option<&common::model::Song>,
    ) -> Rendered {
        let source_sr = buffer.sample_rate;
        let mut onsets = event.onsets.clone();
        onsets.sort_unstable();
        onsets.dedup();
        let mut beat_markers = event.beat_markers.clone();
        beat_markers.sort_by(|a, b| a.locked_beat.total_cmp(&b.locked_beat));
        beat_markers.dedup_by(|a, b| (a.locked_beat - b.locked_beat).abs() < 1e-9);
        let mut schedule = vec![RenderedEvent {
            track_idx: 0,
            cell_clip_id: 0,
            start_beat: 0.0,
            end_beat: event.event_length_beats,
            gate_start_beat: f64::NEG_INFINITY,
            gate_end_beat: f64::INFINITY,
            source_id: 1,
            source_start_frames: event.source_start_frames,
            source_end_frames: event.source_end_frames,
            gain_lin: 1.0,
            pan: 0.0,
            sr_ratio: sample_rate_ratio(source_sr, ENGINE_SR),
            pitch_factor: pitch_factor(event.pitch_semitones),
            pitch_semitones: event.pitch_semitones,
            formant_semitones: event.formant_semitones,
            stream_key: 1,
            needs_engine: false,
            stretch_ratio: stretch_ratio_for(
                event.source_end_frames.saturating_sub(event.source_start_frames),
                source_sr,
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
        }];
        let engines_per_track = count_engines_per_track(&mut schedule);
        let mut sources = HashMap::new();
        sources.insert(1u32, Arc::new(buffer));
        let renderer = AudioClipRenderer { schedule, sources, engines_per_track };
        let n_engines = renderer.engines_per_track.first().copied().unwrap_or(0);
        let mut engines: Vec<StretchEngine> = (0..n_engines)
            .map(|_| StretchEngine::new(ENGINE_SR).expect("stretch engine"))
            .collect();

        let mut accum = vec![(u64::MAX, 0.0f64); 4];
        let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
        let mut render_seq = 0u64;
        let mut r = Rendered { out: Vec::new(), marks: Vec::new() };
        let mut playhead_beats = 0.0_f64;
        while playhead_beats < event.event_length_beats {
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
                0,
                &mut l,
                &mut rr,
                playhead_beats,
                current_bpm,
                ENGINE_SR,
                frames as u32,
                &mut ClipRenderState {
                    repitch_accum: &mut accum,
                    engines: &mut engines,
                    event_l: &mut event_l,
                    event_r: &mut event_r,
                    render_seq: &mut render_seq,
                },
            );
            r.out.extend_from_slice(&l);
            playhead_beats += frames as f64 * f64::from(current_bpm) / (60.0 * f64::from(ENGINE_SR));
        }
        r.marks.push((playhead_beats, r.out.len()));
        r
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

    fn spans_of(
        event: &AudioEvent,
        source_sr: u32,
        tempo_song: Option<&common::model::Song>,
    ) -> Vec<WaveSpan> {
        let tempo = match tempo_song {
            Some(s) => TempoMap::from_song(s),
            None => TempoMap::constant(BPM),
        };
        let mut spans = Vec::new();
        event_wave_spans(event, source_sr, &tempo, 0.0, &mut spans);
        spans
    }

    /// span の無い拍区間 (= gap / 鳴り終わり) が engine でも無音であること。
    ///
    /// `inset` / `eps` は経路で変える: sample 直読の tape / slice は境界丸めぶんの
    /// 64 sample を除けば **厳密に 0**。 spectral は解析窓 (~2880 sample) ぶんの
    /// にじみが境界に出るので、 窓幅の inset と微小振幅の許容が要る。
    fn assert_gaps_are_silent(
        r: &Rendered,
        spans: &[WaveSpan],
        len_beats: f64,
        inset: usize,
        eps: f32,
        label: &str,
    ) {
        let mut silent: Vec<(f64, f64)> = Vec::new();
        if spans[0].start_beat > 0.0 {
            silent.push((0.0, spans[0].start_beat));
        }
        for w in spans.windows(2) {
            if w[1].start_beat > w[0].end_beat {
                silent.push((w[0].end_beat, w[1].start_beat));
            }
        }
        let last_end = spans[spans.len() - 1].end_beat;
        if last_end < len_beats {
            silent.push((last_end, len_beats));
        }
        for (b0, b1) in silent {
            let lo = r.index_at_beat(b0) + inset;
            let hi = r.index_at_beat(b1).min(r.out.len()).saturating_sub(inset);
            if hi <= lo {
                continue;
            }
            for (k, v) in r.out[lo..hi].iter().enumerate() {
                assert!(
                    v.abs() < eps,
                    "{label}: {b0}..{b1} 拍は無音のはず、 out[{}] = {v}",
                    lo + k
                );
            }
        }
    }

    /// span が無い拍区間の **エネルギー**が、鳴っている区間に対して十分小さいこと
    /// (spectral 経路用)。 立ち上がり前後 `lookahead` サンプルは解析窓のにじみが
    /// 乗るので除外する。
    fn assert_gaps_are_quiet(
        r: &Rendered,
        spans: &[WaveSpan],
        len_beats: f64,
        lookahead: usize,
        label: &str,
    ) {
        let rms = |lo: usize, hi: usize| -> f64 {
            if hi <= lo {
                return 0.0;
            }
            let s: f64 = r.out[lo..hi].iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
            (s / (hi - lo) as f64).sqrt()
        };
        // 基準 = 最初の span の中央付近 (確実に鳴っている区間)。
        let s0 = &spans[0];
        let mid = r.index_at_beat((s0.start_beat + s0.end_beat) * 0.5);
        let signal = rms(mid.saturating_sub(2_000), (mid + 2_000).min(r.out.len()));
        assert!(signal > 1e-3, "{label}: 基準区間が鳴っていない: {signal}");

        let mut gaps: Vec<(f64, f64)> = Vec::new();
        if spans[0].start_beat > 0.0 {
            gaps.push((0.0, spans[0].start_beat));
        }
        for w in spans.windows(2) {
            if w[1].start_beat > w[0].end_beat {
                gaps.push((w[0].end_beat, w[1].start_beat));
            }
        }
        let last_end = spans[spans.len() - 1].end_beat;
        if last_end < len_beats {
            gaps.push((last_end, len_beats));
        }
        for (b0, b1) in gaps {
            let lo = r.index_at_beat(b0) + lookahead;
            let hi = r.index_at_beat(b1).min(r.out.len()).saturating_sub(lookahead);
            if hi <= lo {
                continue;
            }
            let q = rms(lo, hi);
            assert!(
                q < signal * 0.01,
                "{label}: {b0}..{b1} 拍は無音のはず (RMS {q:.2e} vs 鳴っている区間 {signal:.2e})"
            );
        }
    }

    /// **tape / slice 経路**: 出力値そのものが source 位置 (ramp 素材)。
    fn assert_tape_render_matches_spans(
        event: &AudioEvent,
        source_sr: u32,
        tempo_song: Option<&common::model::Song>,
        label: &str,
    ) {
        let r = render_model_event(event, ramp(source_sr), tempo_song);
        let spans = spans_of(event, source_sr, tempo_song);
        assert!(!spans.is_empty(), "{label}: span が 1 本も無い");
        let src_max = (u64::from(source_sr) - 1) as f64;
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
                    (got - expect).abs() < 0.02,
                    "{label}: span {i} ({s:?}) の {f} 地点は source {expect} が鳴るべき、 got {got}"
                );
            }
        }
        // sample 直読なので gap は厳密に 0。
        assert_gaps_are_silent(&r, &spans, event.event_length_beats, 64, 1e-6, label);
    }

    /// **spectral (Stretch) 経路**: 局所 RMS が source 位置のエンベロープに一致する。
    /// 許容は `render_tests` の同種テストと同じ「正規化 source 位置で ±0.07」。
    fn assert_spectral_render_matches_spans(
        event: &AudioEvent,
        source_sr: u32,
        tempo_song: Option<&common::model::Song>,
        label: &str,
    ) {
        let r = render_model_event(event, ramped_sine(source_sr), tempo_song);
        let spans = spans_of(event, source_sr, tempo_song);
        assert!(!spans.is_empty(), "{label}: span が 1 本も無い");
        let full_scale = 1.0 / std::f64::consts::SQRT_2;
        let src_max = (u64::from(source_sr) - 1) as f64;
        for (i, s) in spans.iter().enumerate() {
            let dur = s.end_beat - s.start_beat;
            for f in [0.35_f64, 0.6, 0.85] {
                let idx = r.index_at_beat(s.start_beat + dur * f);
                // RMS 窓 (±2000 sample) が span からはみ出す端は測らない。
                if idx < 2_000 || idx + 2_000 >= r.out.len() {
                    continue;
                }
                let expect = span_source_at(s, f) / src_max;
                let got = local_rms(&r.out, idx, 2_000) / full_scale;
                assert!(
                    (got - expect).abs() < 0.07,
                    "{label}: span {i} ({s:?}) の {f} 地点は source {expect:.3} が鳴るべき、 got {got:.3}"
                );
            }
        }
        // spectral は出力位置より `lookahead_samples()` ぶん先の入力を先読みするので、
        // 素材の立ち上がりがその幅だけ手前からスペクトルに現れる (実測: 先読み
        // 7200 sample @48k に対し ~7200 sample 手前から -90 dB 級のにじみ)。
        // 「1 サンプルも 0」 では表せないので、 **エネルギー比**で見る:
        // span 外の RMS は鳴っている区間の 1% (-40 dB) 未満。
        let lookahead = StretchEngine::new(ENGINE_SR)
            .expect("stretch engine")
            .lookahead_samples() as usize;
        assert_gaps_are_quiet(&r, &spans, event.event_length_beats, lookahead, label);
    }

    // ---- Slice (r.md #41 の本題) ----------------------------------------

    /// 伸ばした Slice clip: trigger は広がるが slice 本体は native rate → gap が空く。
    #[test]
    fn slice_spans_match_render_with_gaps() {
        let ev = slice_event(4.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        let spans = spans_of(&ev, 48_000, None);
        assert_eq!(spans.len(), 4);
        assert!(
            spans[0].end_beat < spans[1].start_beat - 1e-9,
            "伸ばした Slice は gap が空く: {spans:?}"
        );
        assert_tape_render_matches_spans(&ev, 48_000, None, "slice gap");
    }

    /// 詰めた Slice clip: 次 trigger が先に来て前 slice が cut される (gap 無し)。
    #[test]
    fn slice_spans_match_render_with_cuts() {
        let ev = slice_event(1.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        assert_tape_render_matches_spans(&ev, 48_000, None, "slice cut");
    }

    /// 移調した Slice clip: trigger は動かず slice 本体だけ速くなる → gap。
    #[test]
    fn slice_spans_match_render_when_pitched() {
        let ev = slice_event(2.0, 12.0, vec![0, 12_000, 24_000, 36_000]);
        assert_tape_render_matches_spans(&ev, 48_000, None, "slice pitch");
    }

    /// 伸縮なしの Slice clip は隙間ゼロ (= 従来の連続波形と同じ絵)。
    #[test]
    fn slice_spans_match_render_when_not_stretched() {
        let ev = slice_event(2.0, 0.0, vec![0, 12_000, 24_000, 36_000]);
        let spans = spans_of(&ev, 48_000, None);
        for w in spans.windows(2) {
            assert!(
                (w[1].start_beat - w[0].end_beat).abs() < 1e-9,
                "伸縮なしなら隙間ゼロ: {spans:?}"
            );
        }
        assert_tape_render_matches_spans(&ev, 48_000, None, "slice lockstep");
    }

    /// source SR ≠ engine SR でも span と実出力が一致する (SR 補正漏れの回帰検出)。
    #[test]
    fn slice_spans_match_render_at_other_sample_rate() {
        for source_sr in [44_100u32, 96_000] {
            let ev = AudioEvent {
                source_end_frames: u64::from(source_sr),
                ..slice_event(4.0, 0.0, vec![0, u64::from(source_sr) / 4, u64::from(source_sr) / 2])
            };
            assert_tape_render_matches_spans(&ev, source_sr, None, &format!("slice {source_sr}Hz"));
        }
    }

    // ---- tape (Raw / Repitch) --------------------------------------------

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
        assert_tape_render_matches_spans(&ev, 48_000, None, "raw pitched");
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
            assert_tape_render_matches_spans(&ev, 48_000, None, &format!("repitch {semis}"));
        }
    }

    /// 逆再生は窓全体を反転して読む。 span の `reversed` 写像が実出力と一致する。
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
        assert_tape_render_matches_spans(&ev, 48_000, None, "raw reversed");

        let ev = AudioEvent { reversed: true, ..slice_event(4.0, 0.0, vec![0, 12_000, 24_000, 36_000]) };
        assert_tape_render_matches_spans(&ev, 48_000, None, "slice reversed");
    }

    // ---- Stretch (r.md #40 でスペクトル方式に置換) ------------------------

    /// **既定 mode** (`AudioEvent::default()` = `Stretch`) の uniform 写像が
    /// spectral 経路の `u_of` (= `beat × src_frames_per_beat`) と一致する。
    #[test]
    fn stretch_uniform_span_matches_render() {
        for len_beats in [2.0_f64, 4.0] {
            let ev = AudioEvent {
                source_start_frames: 0,
                source_end_frames: SOURCE_FRAMES,
                event_length_beats: len_beats,
                stretch_mode: StretchMode::Stretch,
                ..AudioEvent::default()
            };
            assert_spectral_render_matches_spans(
                &ev,
                48_000,
                None,
                &format!("stretch uniform {len_beats}拍"),
            );
        }
    }

    /// source SR ≠ engine SR でも spectral の時間写像と span が一致する。
    #[test]
    fn stretch_span_matches_render_at_other_sample_rate() {
        for source_sr in [44_100u32, 96_000] {
            let ev = AudioEvent {
                source_start_frames: 0,
                source_end_frames: u64::from(source_sr),
                event_length_beats: 4.0,
                stretch_mode: StretchMode::Stretch,
                ..AudioEvent::default()
            };
            assert_spectral_render_matches_spans(
                &ev,
                source_sr,
                None,
                &format!("stretch {source_sr}Hz"),
            );
        }
    }

    /// 移調 / フォルマントは **周波数軸だけ**の操作なので時間写像を変えない。
    /// span (= 描画) が移調前と 1 ビットも変わらないこと + 実出力の時間位置とも
    /// 一致すること。
    ///
    /// 絶対 RMS では測れない: 純サインを `compensatePitch=true` で移調すると
    /// 「元の包絡 (440 Hz にピーク) を保つ」 ため移動先の倍音が減衰し、 全体の
    /// レベルが落ちる (= フォルマント保持の定義どおりの挙動)。 そこで
    /// **RMS の profile (位置に対する比)** で時間写像だけを見る。 素材の
    /// エンベロープは位置に線形なので、 `rms(f) / rms(f_ref)` は
    /// `source_pos(f) / source_pos(f_ref)` に一致するはず。
    #[test]
    fn stretch_time_mapping_is_independent_of_pitch_and_formant() {
        let base = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Stretch,
            ..AudioEvent::default()
        };
        let plain = spans_of(&base, 48_000, None);
        for (semis, formant) in [(12.0_f32, 0.0_f32), (-7.0, 0.0), (0.0, 12.0), (5.0, -5.0)] {
            let label = format!("stretch pitch {semis} formant {formant}");
            let ev = AudioEvent {
                pitch_semitones: semis,
                formant_semitones: formant,
                ..base.clone()
            };
            let spans = spans_of(&ev, 48_000, None);
            assert_eq!(spans, plain, "{label}: 移調 / フォルマントで span が変わってはいけない");

            let r = render_model_event(&ev, ramped_sine(48_000), None);
            let s = &spans[0];
            let dur = s.end_beat - s.start_beat;
            let probe = |f: f64| -> f64 {
                local_rms(&r.out, r.index_at_beat(s.start_beat + dur * f), 2_000)
            };
            let (rms_lo, rms_ref) = (probe(0.35), probe(0.85));
            assert!(rms_ref > 1e-4, "{label}: 出力が無音では profile が測れない: {rms_ref}");
            let got = rms_lo / rms_ref;
            let want = span_source_at(s, 0.35) / span_source_at(s, 0.85);
            assert!(
                (got - want).abs() < 0.08,
                "{label}: 0.35 / 0.85 地点の音量比は source 位置比 {want:.3} に一致すべき、 got {got:.3}"
            );
        }
    }

    /// warp marker の区分線形写像が spectral 経路の `u_of` と一致する。
    /// **窓手前へ外挿する marker** (= auto-warp 後に左 trim した event) では、
    /// #40 の `u_of` は clamp しないので窓手前は **無音**。 描画も span を張らない
    /// (旧 granular は `.max(0.0)` で先頭 frame を保持していたので、 ここは
    /// #40 で意味論が変わった箇所)。
    #[test]
    fn stretch_warp_span_matches_render() {
        use common::model::BeatMarker;
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
        assert_spectral_render_matches_spans(&ev, 48_000, None, "stretch warp");

        // 左 trim 相当: marker 据え置きで source_start_frames だけ前進 → 前半 2 拍が
        // 窓手前 (u < 0) を指す = 無音。 span も 2 拍地点から始まる。
        let trimmed = AudioEvent {
            source_start_frames: 24_000,
            beat_markers: vec![
                BeatMarker { source_frame: 0, locked_beat: 0.0 },
                BeatMarker { source_frame: 48_000, locked_beat: 4.0 },
            ],
            ..ev
        };
        let spans = spans_of(&trimmed, 48_000, None);
        assert_eq!(spans.len(), 1, "窓手前は span を張らない: {spans:?}");
        assert!(
            (spans[0].start_beat - 2.0).abs() < 1e-6,
            "warp が窓に入る 2 拍地点から鳴る: {spans:?}"
        );
        assert_spectral_render_matches_spans(&trimmed, 48_000, None, "stretch warp trimmed");
    }

    // ---- tempo automation -------------------------------------------------

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
            content_offset_beats: 0.0,
        });
        lane.next_clip_id = 2;
        song.song_lanes.push(lane);
        song
    }

    /// SongTempo automation 下でも描画写像と実出力が一致する。
    /// **native rate 再生 (Raw / Slice 本体) だけ**が current_bpm に反比例するので、
    /// 描画が定数 `song.bpm` 固定だとここで落ちる。 Stretch は beat 領域写像なので
    /// tempo 曲線でも不変 (= #40 が granular の lock-in ring を捨てられた理由)。
    #[test]
    fn spans_match_render_under_tempo_automation() {
        let song = tempo_song(60.0, 60.0, 16.0);
        let ev = AudioEvent {
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            event_length_beats: 4.0,
            stretch_mode: StretchMode::Raw,
            ..AudioEvent::default()
        };
        assert_tape_render_matches_spans(&ev, 48_000, Some(&song), "raw @tempo curve");

        let ev = AudioEvent {
            stretch_mode: StretchMode::Slice,
            onsets: vec![0, 12_000, 24_000, 36_000],
            ..ev
        };
        assert_tape_render_matches_spans(&ev, 48_000, Some(&song), "slice @tempo curve");

        let ev = AudioEvent { stretch_mode: StretchMode::Stretch, onsets: Vec::new(), ..ev };
        assert_spectral_render_matches_spans(&ev, 48_000, Some(&song), "stretch @tempo curve");

        // ramp (60→120 BPM) でも一致する = native rate 側の区分線形化が効いている。
        let ramping = tempo_song(60.0, 120.0, 8.0);
        let ev = AudioEvent { stretch_mode: StretchMode::Raw, ..ev };
        assert_tape_render_matches_spans(&ev, 48_000, Some(&ramping), "raw @tempo ramp");
    }
}

/// `AudioSourceId` は **Song スコープの名前** (project ごとに 1 から再採番) な
/// ので、decode 済みバッファの再利用を id 一致だけで判断してはいけない。
/// 別 project を開くと id 衝突で前 project の音源が鳴り、しかも
/// `has_undecoded_sources` が false を返して decode job も積まれないため
/// 恒久的に直らなかった。再利用の同一性は `AudioSourceBuffer::origin`
/// (decode 元の解決済み絶対パス) が担う。
#[cfg(test)]
mod source_identity_tests {
    use super::*;
    use common::model::AudioSource;

    const ENGINE_SR: u32 = 48_000;

    fn song_with_source(rel: &str) -> Song {
        let mut song = Song::default();
        song.media.audio_sources.insert(
            1,
            AudioSource {
                path: AudioSourcePath::ProjectRelative(std::path::PathBuf::from(rel)),
                sample_rate: ENGINE_SR,
                channels: 1,
                frames: 8,
                original_bpm: None,
                root_key: None,
            },
        );
        song
    }

    fn renderer_with_cached(origin: &Path) -> AudioClipRenderer {
        let mut sources = HashMap::new();
        sources.insert(
            1u32,
            Arc::new(AudioSourceBuffer {
                origin: origin.to_path_buf(),
                sample_rate: ENGINE_SR,
                channels: 1,
                frames: 8,
                samples: vec![vec![0.5; 8]],
            }),
        );
        AudioClipRenderer {
            schedule: Vec::new(),
            sources,
            engines_per_track: Vec::new(),
        }
    }

    /// 同一 project 内の再 compile (BPM 変更 / 編集 / scrub) は従来どおり
    /// decode ゼロで再利用される。
    #[test]
    fn same_origin_is_reused_without_decoding() {
        let dir = Path::new("C:/projects/A");
        let song = song_with_source("samples/kick.wav");
        let prev = renderer_with_cached(&dir.join("samples/kick.wav"));

        let out = compile_audio_schedule(&song, Some(&prev), Some(dir), ENGINE_SR, false);
        assert!(
            out.sources.contains_key(&1),
            "origin 一致なら decode_missing=false でも再利用される"
        );
        assert!(
            !has_undecoded_sources(&song, &out, Some(dir)),
            "再利用できた source は未 decode 扱いにしない"
        );
    }

    /// 別 project を開いた状況: 同じ `AudioSourceId` だが別ファイル。
    /// 前 project のバッファを掴んではならず、decode job が積まれること。
    #[test]
    fn different_origin_is_not_reused_and_is_queued_for_decode() {
        let dir_b = Path::new("C:/projects/B");
        let song_b = song_with_source("samples/snare.wav");
        // 前 project A の同 id バッファが live renderer に残っている状況。
        let prev = renderer_with_cached(Path::new("C:/projects/A/samples/kick.wav"));

        let out = compile_audio_schedule(&song_b, Some(&prev), Some(dir_b), ENGINE_SR, false);
        assert!(
            !out.sources.contains_key(&1),
            "別 project の同 id バッファを id 一致だけで再利用してはいけない"
        );
        assert!(
            has_undecoded_sources(&song_b, &out, Some(dir_b)),
            "再利用できなかった source は decode job の対象になること \
             (ここが false だと恒久的に前 project の音が鳴り続ける)"
        );
    }

    /// `project_dir` が未設定だと `ProjectRelative` は解決できない。decode 対象に
    /// できないので「未 decode」に数えない (数えると decode job が空回りし続ける)。
    #[test]
    fn unresolvable_project_relative_is_not_counted_as_undecoded() {
        let song = song_with_source("samples/kick.wav");
        let out = compile_audio_schedule(&song, None, None, ENGINE_SR, false);
        assert!(out.sources.is_empty());
        assert!(!has_undecoded_sources(&song, &out, None));
    }
}
