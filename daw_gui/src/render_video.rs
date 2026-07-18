//! MP4 render via Windows Media Foundation (`docs/plan_video.md` P8,
//! `docs/plan_text_overlay.md` P2).
//!
//! Iterates output frames at `Song.video_framerate`, builds a
//! `daw_ui_renderer::Scene` for each playhead by uploading the active
//! video frame + image PiP layers as wgpu textured quads, asks the
//! `OffscreenRenderer` to composite at project resolution, reads the
//! RGBA back, converts to NV12, and writes through `IMFSinkWriter` to
//! an on-disk H.264 mp4. When an `audio_wav_path` is provided, also
//! opens it via `hound`, encodes PCM Float32 to AAC via the same sink
//! writer, and muxes a single output mp4.
//!
//! `plan_text_overlay.md` P2 swapped the CPU `blit_layer` pipeline for
//! the GPU OffscreenRenderer so:
//! - preview + export use the exact same composite shader
//! - image rotation (gui_01 #047) renders identically in both paths
//! - text overlays (P3) plug into the same `Scene::push_text` path
//!
//! MVP scope (simplest viable):
//! - H.264 video at project resolution / framerate, ~5 Mbit/s
//! - AAC stereo audio at 192 Kbit/s (when wav supplied)
//! - Synchronous render on the caller thread (= UI hangs for the
//!   duration). Real-time hiccup acceptable for offline render.
//! - wgpu sRGB linear blend (matches preview).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use common::model::{ImageSourceId, Song, VideoSourceId, VideoSourcePath};
use common::tempo_map::TempoMap;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene, TextureHandle, TexturedQuad};

use crate::video_playback::VideoPlaybackEngine;

/// Render configuration. All fields are derived from `Song` except
/// `output_path` / `audio_wav_path` which the caller supplies from
/// the export dialog.
pub struct RenderConfig<'a> {
    pub song: &'a Song,
    pub project_dir: Option<&'a Path>,
    pub output_path: &'a Path,
    /// Optional sibling WAV produced by `Export WAV...`. When `Some`,
    /// the output mp4 carries an AAC audio track muxed in by the same
    /// `IMFSinkWriter` pass.
    pub audio_wav_path: Option<&'a Path>,
    /// Average video bitrate in bits-per-second. ~5 Mbit/s is a sane
    /// default for 1080p MV-style content (= visually transparent on
    /// most YouTube uploads, file size around 40 MB / minute).
    pub video_bitrate: u32,
    /// Average AAC audio bitrate. 192 kbit/s is the YouTube / SoundCloud
    /// upload sweet spot for music.
    pub audio_bitrate: u32,
    /// user-chosen export window in **beats**, `(start_beat,
    /// end_beat)`. `None` = whole song (frame 0 → `length_beats`). When
    /// `Some`, the frame loop renders only `[start_beat, end_beat)`, with
    /// the first output frame mapped to `start_beat`. The muxed
    /// `audio_wav_path` must already be trimmed to the same window (the
    /// audio export writes only its requested frame range starting at
    /// sample 0), so video and audio stay aligned at output frame 0.
    pub range_beats: Option<(f64, f64)>,
    /// per-export output resolution `(width, height)` chosen in the
    /// export dialog. `None` = use the project canvas (`Song.video_resolution`).
    /// When `Some`, the `OffscreenRenderer` composites directly at this size, so
    /// clips are aspect-fit (letterboxed) onto the chosen canvas with no extra
    /// resample — identical to changing the project canvas, but ephemeral (the
    /// project / preview are left untouched).
    pub output_resolution: Option<(u32, u32)>,
    /// per-export output frame rate chosen in the export dialog.
    /// `None` = use the project frame rate (`Song.video_framerate`). The frame
    /// loop steps at this rate (one composited frame per output tick), so it is
    /// purely an output-timeline parameter — audio (beat→sample) is unaffected
    /// and A/V stay aligned at output frame 0.
    pub output_framerate: Option<f32>,
}

impl<'a> RenderConfig<'a> {
    pub fn new(song: &'a Song, output_path: &'a Path) -> Self {
        Self {
            song,
            project_dir: None,
            output_path,
            audio_wav_path: None,
            video_bitrate: 5_000_000,
            audio_bitrate: 192_000,
            range_beats: None,
            output_resolution: None,
            output_framerate: None,
        }
    }

    pub fn with_project_dir(mut self, dir: Option<&'a Path>) -> Self {
        self.project_dir = dir;
        self
    }

    pub fn with_audio_wav(mut self, path: Option<&'a Path>) -> Self {
        self.audio_wav_path = path;
        self
    }

    /// restrict the rendered window to `[start_beat, end_beat)`.
    /// `None` renders the whole song (default).
    pub fn with_range_beats(mut self, range: Option<(f64, f64)>) -> Self {
        self.range_beats = range;
        self
    }

    /// override the output resolution for this export. `None` keeps
    /// the project canvas (`Song.video_resolution`).
    pub fn with_output_resolution(mut self, resolution: Option<(u32, u32)>) -> Self {
        self.output_resolution = resolution;
        self
    }

    /// override the output frame rate for this export. `None` keeps
    /// the project frame rate (`Song.video_framerate`).
    pub fn with_output_framerate(mut self, framerate: Option<f32>) -> Self {
        self.output_framerate = framerate;
        self
    }

    /// the effective output resolution — the per-export override if
    /// set, else the project canvas. SSoT for "what size this export encodes at".
    #[must_use]
    pub fn resolved_resolution(&self) -> (u32, u32) {
        self.output_resolution.unwrap_or(self.song.video_resolution)
    }

    /// the effective output frame rate — the per-export override if
    /// set, else the project frame rate.
    #[must_use]
    pub fn resolved_framerate(&self) -> f32 {
        self.output_framerate.unwrap_or(self.song.video_framerate)
    }
}

/// Render the project to an mp4 at `cfg.output_path`. Synchronous,
/// runs to completion (or returns Err on the first stream-level
/// failure). Progress callbacks: not exposed on this signature
/// — the caller can wrap the call in a thread with a status_message
/// poll if they want a UI progress bar. For MVP we just block the
/// GUI thread.
pub fn render_mp4(cfg: &RenderConfig) -> Result<RenderStats, String> {
    render_mp4_cancellable(
        cfg,
        &std::sync::atomic::AtomicBool::new(false),
        &mut |_, _| {},
    )
}

/// `render_mp4` の進捗通知 + キャンセル対応版。background thread で走らせ、
/// `on_progress(done, total)` を毎フレーム呼ぶ。`cancel` が立ったら出力を
/// 破棄して `Err("export cancelled")` を返す（同期 export は GUI スレッドを
/// 長時間ブロックするため、 呼び出し側はこちらを別スレッドで使う）。
pub fn render_mp4_cancellable(
    cfg: &RenderConfig,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<RenderStats, String> {
    // output dims/fps come from the per-export override (export
    // dialog) when set, else the project canvas (`Song`). The OffscreenRenderer
    // composites directly at `out_w x out_h`, so a chosen size just re-letterboxes
    // the clips onto that canvas (no extra resample).
    let (out_w, out_h) = cfg.resolved_resolution();
    if out_w == 0 || out_h == 0 {
        return Err(format!(
            "invalid export video_resolution {out_w}x{out_h}"
        ));
    }
    let framerate = cfg.resolved_framerate();
    if framerate <= 0.0 {
        return Err(format!("invalid export video_framerate {framerate}"));
    }
    if cfg.song.bpm <= 0.0 {
        return Err(format!("invalid project bpm {}", cfg.song.bpm));
    }

    // In-process libav encoder (NVENC H.264 + optional AAC) → mp4 mux,
    // replacing the MF sink writer (docs/plan_video_export_libav.md). The
    // audio spec is probed from the optional export WAV; its samples are
    // streamed in after the video loop (the muxer interleaves by DTS).
    let audio_spec = if let Some(wav_path) = cfg.audio_wav_path {
        let spec = hound::WavReader::open(wav_path)
            .map_err(|e| format!("open audio wav {}: {e}", wav_path.display()))?
            .spec();
        if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
            return Err(format!(
                "audio wav must be PCM Float32 (got {:?} {}-bit)",
                spec.sample_format, spec.bits_per_sample
            ));
        }
        if spec.channels == 0 || spec.sample_rate == 0 {
            return Err(format!(
                "audio wav has invalid channels={} sample_rate={}",
                spec.channels, spec.sample_rate
            ));
        }
        Some(crate::libav_encoder::AudioSpec {
            sample_rate: spec.sample_rate,
            channels: spec.channels as u32,
            bitrate: cfg.audio_bitrate,
        })
    } else {
        None
    };

    // Encode on a dedicated worker thread so the GPU composite + readback of
    // frame N+1 overlaps the NVENC encode + mux of frame N (Phase 2, daw_01
    // side; the readback async-overlap itself awaits the gui_01 API). The libav
    // encoder is not `Send`, so it is created and owned entirely on the worker —
    // the main thread only ships RGBA buffers over a bounded channel (which also
    // back-pressures if the encoder ever falls behind).
    enum EncCmd {
        Video(Vec<u8>),
        FinishWithAudio(Option<PathBuf>),
    }
    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<EncCmd>(3);
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    let enc_output = cfg.output_path.to_path_buf();
    let video_bitrate = cfg.video_bitrate;
    let enc_handle = std::thread::spawn(move || -> Result<(), String> {
        let mut encoder = match crate::libav_encoder::LibavEncoder::new(
            &enc_output,
            out_w,
            out_h,
            framerate,
            video_bitrate,
            audio_spec,
        ) {
            Ok(e) => {
                let _ = init_tx.send(Ok(()));
                e
            }
            Err(e) => {
                let _ = init_tx.send(Err(e.clone()));
                return Err(e);
            }
        };
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                EncCmd::Video(rgba) => encoder.push_video_rgba(&rgba)?,
                EncCmd::FinishWithAudio(wav) => {
                    if let Some(w) = wav {
                        push_wav_audio(&mut encoder, &w)?;
                    }
                    return encoder.finish();
                }
            }
        }
        // Channel closed before a Finish command = user cancel: drop the
        // encoder without writing the trailer.
        Ok(())
    });
    // Surface a constructor failure (e.g. no usable encoder) before the loop.
    init_rx
        .recv()
        .map_err(|_| "encoder thread died during init".to_string())?
        .map_err(|e| format!("libav encoder init: {e}"))?;

    // OffscreenRenderer: own its wgpu device + pipelines independent
    // from the main daw_gui window. Canvas is exactly project
    // resolution so video / image layer rects map 1:1 to output px.
    let mut offscreen = OffscreenRenderer::new(out_w, out_h)
        .map_err(|e| format!("OffscreenRenderer::new({out_w}x{out_h}): {e:?}"))?;
    tracing::info!(out_w, out_h, "export: OffscreenRenderer created");
    // Per-source GPU texture caches. Video texture is uploaded fresh
    // each frame (= BGRA bytes from the CPU decoder); image texture is
    // uploaded once at startup and reused for every frame the layer is
    // visible.
    let mut video_textures: HashMap<VideoSourceId, (TextureHandle, u32, u32)> = HashMap::new();
    let mut image_textures: HashMap<ImageSourceId, (TextureHandle, u32, u32)> = HashMap::new();

    // docs/plan_image_overlay.md §P3: decode each project image once
    // up-front (= keyed by ImageSourceId), upload to a persistent
    // wgpu texture, then push a textured-quad per frame the image is
    // visible. The typical MV has < 10 images, each < 4 MB at 1080p,
    // so keeping all of them resident in GPU memory is fine.
    for (image_source_id, source) in &cfg.song.media.image_sources {
        let abs = match &source.path {
            common::model::ImageSourcePath::Absolute(p) => p.clone(),
            common::model::ImageSourcePath::ProjectRelative(rel) => match cfg.project_dir {
                Some(d) => d.join(rel),
                None => {
                    tracing::warn!(
                        image_source_id,
                        rel = ?rel,
                        "image is project-relative but project_dir is None; skipping"
                    );
                    continue;
                }
            },
        };
        match image::open(&abs) {
            Ok(dynamic) => {
                let rgba = dynamic.into_rgba8();
                let (w, h) = rgba.dimensions();
                let handle = offscreen.create_texture(w, h);
                offscreen.upload_texture_rgba(handle, rgba.as_raw());
                image_textures.insert(*image_source_id, (handle, w, h));
            }
            Err(e) => tracing::warn!(
                image_source_id,
                path = %abs.display(),
                error = %e,
                "image decode failed; skipping layer"
            ),
        }
    }

    // In-process libav software decoder (docs/plan_video_export_libav.md
    // Phase 3): decodes every source — incl. 10-bit H.264 / HEVC / AV1 — to
    // BGRA8 with avcodec + swscale. Replaces the MF SW decode + ffmpeg.exe
    // fallback, which silently dropped 10-bit video in the export's SW path.
    let mut decoder = crate::libav_decoder::LibavVideoDecoder::new();
    // render only the chosen beat window (default = whole song).
    // `start_beat` becomes output frame 0; the audio WAV muxed in is already
    // trimmed to the same window starting at its own sample 0, so A/V stay
    // aligned. 窓解決は `resolve_render_window` に一本化 (length_beats で clamp
    // しないのが要点 — r.md #32、下記関数の doc 参照)。
    let (start_beat, end_beat) = resolve_render_window(cfg.range_beats, cfg.song.length_beats);
    // M2 (r.md #8): frame↔beat は tempo automation を積分する `TempoMap` で写像する
    // (audio export・preview と同一)。 constant-bpm 換算のままだと SongTempo curve の
    // ある曲で映像が tempo 積分済みの audio から drift する (A/V desync)。
    let tempo_map = TempoMap::from_song(cfg.song);
    let start_secs = tempo_map.beat_to_seconds(start_beat);
    let window_seconds = tempo_map.beat_to_seconds(end_beat) - start_secs;
    let total_frames = (window_seconds * f64::from(framerate)).ceil() as u64;
    let mut scene = Scene::new();
    // Opaque black backdrop — matches the pre-P2 CPU path (= the canvas
    // was cleared to (0,0,0,255) before any blit). Letterboxed video
    // layers leave bars; uncovered regions read as black.
    scene.clear_color = wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

    tracing::info!(
        total_frames,
        out_w,
        out_h,
        start_beat,
        end_beat,
        is_range = cfg.range_beats.is_some(),
        "export: starting frame loop"
    );
    let mut cancelled = false;
    // Phase 2b (gui_01 #077 submit_readback/finish_readback): 1-frame-ahead
    // async readback. At the top of iteration N, `pending` holds frame N-1's
    // submitted readback (its GPU copy ran during iteration N-1). We submit
    // frame N without blocking, then collect N-1 and hand it to the encode
    // worker. So composite+submit(N) ∥ GPU readback(N-1) ∥ worker encode(N-2)
    // overlap — a 3-stage pipeline with the Phase 2a encode worker.
    // docs/plan_modulation.md §7: load the baked modulation env sidecar written
    // next to the audio WAV, so visual modulation renders the same as the live
    // preview. Absent / unreadable → empty (no modulation, curve/base only).
    let mod_sidecar = cfg
        .audio_wav_path
        .map(common::mod_sidecar::ModEnvSidecar::sidecar_path)
        .and_then(|p| common::mod_sidecar::ModEnvSidecar::read(&p).ok())
        .unwrap_or_default();
    let mut mod_scalars_buf: Vec<f32> = Vec::new();
    // トラック映像効果の GPU 実行基盤 (preview と同一の VideoFxEngine)。
    // pipeline cache + ping-pong pool を frame 跨ぎ保持。export 終了で offscreen
    // と共に drop (= pool target も解放)。
    let mut fx_engine = crate::video_fx::VideoFxEngine::new();

    let mut pending = None;
    for frame_index in 0..total_frames {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(frame_index, total_frames, "export: cancelled by user");
            cancelled = true;
            break;
        }
        on_progress(frame_index, total_frames);
        // Output frame 0 maps to `start_beat` (= the window origin), so a
        // range export starts compositing at the chosen position.
        let frame_seconds = frame_index as f64 / f64::from(framerate);
        let playhead_beat = tempo_map.seconds_to_beat(start_secs + frame_seconds);
        // docs/plan_modulation.md §7: sample the baked follower envelopes at
        // this frame's beat (step / sample-and-hold), same composition as live.
        mod_sidecar.sample_at(playhead_beat, &mut mod_scalars_buf);
        scene.primitives.clear();
        build_frame_scene(
            cfg.song,
            cfg.project_dir,
            &mut decoder,
            &mut offscreen,
            &mut fx_engine,
            &mut video_textures,
            &image_textures,
            playhead_beat,
            start_secs + frame_seconds,
            &mod_scalars_buf,
            out_w,
            out_h,
            &mut scene,
        );
        // Submit frame N's composite + readback without blocking (poll せず即
        // return)。reusing `scene` next iteration is fine — submit captures it.
        let next = offscreen
            .submit_readback(&scene)
            .map_err(|e| format!("submit_readback frame {frame_index}: {e:?}"))?;
        // Collect + hand off the previous frame (its GPU readback overlapped
        // this iteration's composite). A send error means the encode worker
        // died mid-stream; its real error is surfaced by `join` below.
        if let Some(prev) = pending.take() {
            let rgba = offscreen
                .finish_readback(prev)
                .map_err(|e| format!("finish_readback frame {frame_index}: {e:?}"))?;
            if cmd_tx.send(EncCmd::Video(rgba)).is_err() {
                break;
            }
        }
        pending = Some(next);
    }
    // Drain the last in-flight readback (the final composited frame) on the
    // normal path. On cancel we drop the token — `offscreen` frees the ring on
    // drop, invalidating it.
    if !cancelled && let Some(prev) = pending.take() {
        let rgba = offscreen
            .finish_readback(prev)
            .map_err(|e| format!("finish_readback (final): {e:?}"))?;
        let _ = cmd_tx.send(EncCmd::Video(rgba));
    }

    // Release every cached GPU texture before `offscreen` drops (cancel /
    // 正常終了の両方で実行)、 wgpu device shutdown がクリーンな texture store
    // を見るように。
    for (_, (handle, _, _)) in video_textures.drain() {
        offscreen.destroy_texture(handle);
    }
    for (_, (handle, _, _)) in image_textures.drain() {
        offscreen.destroy_texture(handle);
    }

    if cancelled {
        // partial mp4 は finalize せず破棄する（trailer 未書き込み = 壊れた
        // 中途ファイルを残さない）。worker は channel close で encoder を drop
        // （trailer なし）→ join 後に削除。
        drop(cmd_tx);
        let _ = enc_handle.join();
        let _ = std::fs::remove_file(cfg.output_path);
        return Err("export cancelled".to_string());
    }

    // Normal end: tell the worker to stream the WAV audio into the AAC encoder
    // and write the mp4 trailer, then join and propagate any encode error.
    let audio_wav = cfg.audio_wav_path.map(Path::to_path_buf);
    let _ = cmd_tx.send(EncCmd::FinishWithAudio(audio_wav));
    drop(cmd_tx);
    enc_handle
        .join()
        .map_err(|_| "encoder thread panicked".to_string())?
        .map_err(|e| format!("export encode: {e}"))?;

    on_progress(total_frames, total_frames);
    Ok(RenderStats {
        frames_written: total_frames,
        output_path: cfg.output_path.to_path_buf(),
    })
}

/// Stream the export WAV (PCM Float32, interleaved, native rate) into the
/// encoder's AAC stream in ~8k-sample chunks. The encoder buffers partial AAC
/// frames internally, so chunk size only affects call granularity.
fn push_wav_audio(
    encoder: &mut crate::libav_encoder::LibavEncoder,
    wav_path: &Path,
) -> Result<(), String> {
    let mut reader = hound::WavReader::open(wav_path)
        .map_err(|e| format!("reopen audio wav {}: {e}", wav_path.display()))?;
    const CHUNK: usize = 8192;
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK);
    for sample in reader.samples::<f32>() {
        buf.push(sample.map_err(|e| format!("audio sample read: {e}"))?);
        if buf.len() >= CHUNK {
            encoder
                .push_audio_interleaved(&buf)
                .map_err(|e| format!("push audio: {e}"))?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        encoder
            .push_audio_interleaved(&buf)
            .map_err(|e| format!("push audio: {e}"))?;
    }
    Ok(())
}

/// Returned by `render_mp4` so the caller can populate `status_message`
/// with a useful "wrote N frames to PATH" line.
#[derive(Debug, Clone)]
pub struct RenderStats {
    pub frames_written: u64,
    pub output_path: PathBuf,
}

/// Build the `Scene` for one playhead beat by walking active video +
/// image layers and pushing one textured-quad per layer. Bottom-up
/// order (= `active_*_sources_at` returns `z_index` ascending) drives
/// gui_01's call-order interleave so the top track wins at
/// `alpha = 1.0` and crossfades blend at intermediate alphas. Image
/// rotation is preserved through `TexturedQuad.rotation_radians`
/// (gui_01 #047) so the export now matches preview rotation byte-for-
/// byte.
#[allow(clippy::too_many_arguments)]
fn build_frame_scene(
    song: &Song,
    project_dir: Option<&Path>,
    decoder: &mut crate::libav_decoder::LibavVideoDecoder,
    offscreen: &mut OffscreenRenderer,
    fx_engine: &mut crate::video_fx::VideoFxEngine,
    video_textures: &mut HashMap<VideoSourceId, (TextureHandle, u32, u32)>,
    image_textures: &HashMap<ImageSourceId, (TextureHandle, u32, u32)>,
    playhead_beat: f64,
    playhead_secs: f64,
    mod_scalars: &[f32],
    out_w: u32,
    out_h: u32,
    scene: &mut Scene,
) {
    // 前 frame の効果 target を解放 (前 frame は submit_readback で sample 済み)。
    // 今 frame の apply_chain が同寸でも別 target を払い出し、レイヤー間衝突を防ぐ。
    fx_engine.end_frame(offscreen);
    // 時間系効果の P.time（秒）。preview と同じ song 時間 (tempo 積分済み、
    // caller の frame→秒がそのまま真値) を渡して時間系効果も一致させる。
    fx_engine.set_time(playhead_secs as f32);
    // 動画 / PiP 画像 / テキストを owning track
    // ごとに 1 枚へ合成する (preview と同一の per-track composite。byte parity)。各 track の
    // 視覚アイテムを bucket に集め、track 順 (z 順) に共通の composite_and_place で描く。
    use crate::group_compose::CompositeItem;
    let canvas = (out_w.max(1) as f32, out_h.max(1) as f32);
    let mut buckets: HashMap<u32, Vec<CompositeItem>> = HashMap::new();

    // 動画フレーム (in-process libav software decode → BGRA texture) → owning track bucket。
    let video_layers = VideoPlaybackEngine::active_sources_at(song, playhead_beat);
    for layer in video_layers {
        let Some(path) =
            resolve_video_source_path(song, layer.video_source_id, project_dir)
        else {
            continue;
        };
        // decode errors はログのみ (黙って黒くしない。10-bit で MF/ffmpeg.exe が踏んだ罠)。
        let frame = match decoder.decode_at(layer.video_source_id, &path, layer.source_micros) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    video_source_id = layer.video_source_id,
                    source_micros = layer.source_micros,
                    error = %e,
                    "export: video decode failed; layer skipped"
                );
                continue;
            }
        };
        let (bgra, width, height) = (&frame.bgra, frame.width, frame.height);
        // Get-or-create the per-source BGRA texture, recreating on dimension change。
        let recreate = match video_textures.get(&layer.video_source_id) {
            Some((_, w, h)) => *w != width || *h != height,
            None => true,
        };
        if recreate {
            if let Some((old, _, _)) = video_textures.remove(&layer.video_source_id) {
                offscreen.destroy_texture(old);
            }
            let handle = offscreen.create_texture_bgra(width, height);
            video_textures.insert(layer.video_source_id, (handle, width, height));
        }
        let texture = video_textures
            .get(&layer.video_source_id)
            .map(|(h, _, _)| *h)
            .expect("just inserted");
        offscreen.upload_texture_bgra(texture, bgra);
        let dest = crate::group_compose::aspect_fit_norm(canvas, (width as f32, height as f32));
        buckets.entry(layer.owning_track_id).or_default().push(CompositeItem::Quad {
            texture,
            dest,
            alpha: layer.alpha,
            rotation_radians: 0.0,
        });
    }

    // active visual groups (preview と同一 gate `active_visual_groups`、SSoT)。
    // mod_scalars は render_mp4 が baked env sidecar から sample (空 = 変調なし)。
    let active_groups = crate::group_compose::active_visual_groups(song, playhead_beat, mod_scalars);

    // PiP 画像 → 親が active group ならその group bucket へ吸収、さもなくば owning track bucket。
    let image_layers =
        crate::image_compose::active_image_sources_at(song, playhead_beat, mod_scalars);
    for layer in image_layers {
        let Some((texture, _iw, _ih)) = image_textures.get(&layer.image_source_id) else {
            continue; // not decoded / failed import
        };
        let target_track = song
            .track_by_id(layer.owning_track_id)
            .and_then(|t| t.parent_group_id)
            .filter(|g| active_groups.contains_key(g))
            .unwrap_or(layer.owning_track_id);
        buckets.entry(target_track).or_default().push(CompositeItem::Quad {
            texture: *texture,
            dest: (layer.x, layer.y, layer.w, layer.h),
            alpha: layer.alpha,
            rotation_radians: layer.rotation_radians,
        });
    }

    // テキスト → owning track bucket (合成画に焼き込んで track 効果を乗せる)。
    let text_layers =
        crate::text_compose::active_text_sources_at(song, playhead_beat, mod_scalars);
    for tf in text_layers {
        buckets.entry(tf.owning_track_id).or_default().push(CompositeItem::Text(tf));
    }

    // track 順 (bottom→top = rev) に共通 composite_and_place で描く (preview と同一 SSoT、
    // byte parity)。export は選択オーバーレイ無し。
    let project_box = (0.0, 0.0, out_w as f32, out_h as f32);
    for track in song.tracks.iter().rev() {
        let items = buckets.remove(&track.id).unwrap_or_default();
        if items.is_empty() {
            continue; // export は overlay 不要なので空 bucket は skip。
        }
        // 配置 transform は Transform device から解決（preview と同一 SSoT）。
        let transform = crate::video_fx::resolve_track_transform(song, track, playhead_beat, mod_scalars);
        let fx = crate::video_fx::resolve_track_effects(song, track, playhead_beat, mod_scalars);
        let tc = crate::group_compose::TrackComposite {
            track_id: track.id,
            items,
            transform,
            fx,
            selected: false,
        };
        crate::group_compose::composite_and_place(
            &tc,
            project_box,
            (out_w, out_h),
            offscreen,
            fx_engine,
            scene,
        );
    }

    // マスター映像チェーン。全トラック合成後の scene を 1 枚の master
    // canvas へ集約 → master fx をチェーン順適用 → scene を master 1 quad で置換（preview と
    // 同一 SSoT）。空なら何もしない。
    let master_fx = crate::video_fx::resolve_master_effects(song, playhead_beat, mod_scalars);
    if !master_fx.is_empty() {
        match offscreen.composite_scene_to_texture(scene, out_w, out_h) {
            Ok(handle) => {
                let handle = fx_engine.apply_chain(
                    offscreen,
                    handle,
                    out_w,
                    out_h,
                    &master_fx,
                    crate::video_fx::MASTER_CHAIN_KEY,
                );
                scene.primitives.clear();
                scene.push_textured_quad(TexturedQuad {
                    rect: Rect::new(0.0, 0.0, out_w as f32, out_h as f32),
                    texture: handle,
                    alpha: 1.0,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: None,
                    rotation_radians: 0.0,
                    rotation_pivot: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "master 映像 composite 失敗 (export)"),
        }
    }
}

/// 書き出す拍窓 `[start_beat, end_beat)` を解決する。output frame 0 が
/// `start_beat` に対応し、muxed audio WAV も同じ窓に trim 済みなので A/V が揃う。
///
/// `Some((s, e))` は user 指定 / ループ範囲 (レンジピッカーで確定した値) を
/// **そのまま** 使う。負値・非有限だけ弾いて順序を正すだけで、**`length_beats`
/// で clamp しない**。
///
/// これが r.md #32 の要点: ループ範囲は `length_beats` を超えられ (loop_end >
/// length_beats)、content 自体も `length_beats` 超に置ける。audio 書き出し
/// (`AppData::export_beats_to_frames`) は `length_beats` で clamp せず raw な拍
/// 範囲を frame へ換算するので、ここで video だけ `length_beats` に clamp すると
/// **映像だけが短く切れて A/V 長が食い違う**。実例: loop `[8, 260]`・length
/// `64`・BPM 140 の曲で、旧実装は end を 64 に clamp して窓 `[8, 64]` = 56 拍 =
/// 24.0s の映像しか出さず、audio は raw `[8, 260]` = 252 拍 = 108s だった
/// (content は beat 260 まで在るのに映像が 24s で凍結)。
///
/// `None` は全曲 = `[0, length_beats)` (レンジピッカーで「全曲」を選んだとき)。
fn resolve_render_window(range_beats: Option<(f64, f64)>, length_beats: f64) -> (f64, f64) {
    // 負値・NaN/Inf を 0 に潰す (GUI も検証済みだが防御)。`f64::max` は
    // 非 NaN 側を返すので NaN.max(0.0) == 0.0。
    let sane = |b: f64| if b.is_finite() { b.max(0.0) } else { 0.0 };
    match range_beats {
        Some((s, e)) => {
            let (s, e) = (sane(s), sane(e));
            (s.min(e), s.max(e))
        }
        None => (0.0, sane(length_beats)),
    }
}

fn resolve_video_source_path(
    song: &Song,
    video_source_id: VideoSourceId,
    project_dir: Option<&Path>,
) -> Option<PathBuf> {
    let src = song.media.video_sources.get(&video_source_id)?;
    match &src.path {
        VideoSourcePath::Absolute(p) => Some(p.clone()),
        VideoSourcePath::ProjectRelative(rel) => project_dir.map(|d| d.join(rel)),
    }
}

// 動画フレームの aspect-fit は `crate::group_compose::aspect_fit_norm`（normalized）に
// 統一 (Wave2 で per-track 合成 1 枚化、テストは group_compose 側)。

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #32 回帰: 書き出し窓は `length_beats` を超えるループ / 範囲もそのまま
    /// 尊重し、audio 側と同じ窓を返す。旧実装は end を `length_beats` に clamp して
    /// いたため、loop `[8, 260]`・length `64` の曲で映像だけ `[8, 64]` = 24s に
    /// truncate され、audio は `[8, 260]` = 108s と割れていた (映像が 24 秒で凍結)。
    #[test]
    fn render_window_honors_range_beyond_length_beats() {
        // r.md #32 の実データ: loop_end (260) > length_beats (64) を clamp しない。
        assert_eq!(
            resolve_render_window(Some((8.0, 260.0)), 64.0),
            (8.0, 260.0),
            "範囲は length_beats を超えても clamp せず尊重する"
        );
        // 全曲 (レンジピッカー「全曲」) は length_beats まで。
        assert_eq!(resolve_render_window(None, 64.0), (0.0, 64.0));
        // 逆順 (end < start) は昇順に正す。
        assert_eq!(resolve_render_window(Some((260.0, 8.0)), 64.0), (8.0, 260.0));
        // 負値は 0 へ、上限は clamp しない。
        assert_eq!(resolve_render_window(Some((-5.0, 100.0)), 64.0), (0.0, 100.0));
        // 非有限 (NaN/Inf) は 0 に潰して防御する。
        assert_eq!(resolve_render_window(Some((f64::NAN, 10.0)), 64.0), (0.0, 10.0));
        assert_eq!(
            resolve_render_window(Some((5.0, f64::INFINITY)), 64.0),
            (0.0, 5.0),
            "Inf は 0 に潰れ、順序正規化で (0, 5) になる"
        );
    }

    /// the export output dims/fps default to the project canvas, but
    /// a per-export override (export dialog) wins when set. This is the SSoT the
    /// render loop reads — project values stay untouched (ephemeral override).
    #[test]
    fn resolved_dims_use_override_else_project() {
        let song = Song {
            video_resolution: (1920, 1080),
            video_framerate: 30.0,
            ..Song::default()
        };
        let out = std::path::PathBuf::from("out.mp4");

        // No override → project canvas / project fps.
        let base = RenderConfig::new(&song, &out);
        assert_eq!(base.resolved_resolution(), (1920, 1080));
        assert_eq!(base.resolved_framerate(), 30.0);

        // Resolution override only → fps still falls back to project.
        let res_only = RenderConfig::new(&song, &out).with_output_resolution(Some((1280, 720)));
        assert_eq!(res_only.resolved_resolution(), (1280, 720));
        assert_eq!(res_only.resolved_framerate(), 30.0);

        // Both overridden (e.g. vertical 9:16 @ 60).
        let both = RenderConfig::new(&song, &out)
            .with_output_resolution(Some((1080, 1920)))
            .with_output_framerate(Some(60.0));
        assert_eq!(both.resolved_resolution(), (1080, 1920));
        assert_eq!(both.resolved_framerate(), 60.0);
    }

    /// Test helper: generate a `w x h` blue H.264 source mp4 via the `ffmpeg`
    /// CLI (1 second @ 30fps). Returns the written source path.
    fn gen_blue_source(
        ffmpeg: &std::path::Path,
        dir: &std::path::Path,
        w: u32,
        h: u32,
    ) -> std::path::PathBuf {
        let src_mp4 = dir.join("src.mp4");
        let lavfi = format!("color=c=blue:size={w}x{h}:duration=1:rate=30");
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", &lavfi,
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                src_mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success());
        src_mp4
    }

    /// Test helper: a 1-video-track song, 1 clip 4 beats @ 120 BPM (= 2 seconds),
    /// project canvas `(w, h)` @ 30fps, pointing at `src_mp4` (1s source → the
    /// second half renders the last-frame fallback, acceptable for smoke).
    fn build_one_video_song(src_mp4: std::path::PathBuf, w: u32, h: u32) -> Song {
        let mut song = Song {
            bpm: 120.0,
            length_beats: 4.0,
            video_resolution: (w, h),
            video_framerate: 30.0,
            ..Song::default()
        };
        let vsrc_id = song.alloc_video_source_id();
        song.media.video_sources.insert(
            vsrc_id,
            common::model::VideoSource {
                path: common::model::VideoSourcePath::Absolute(src_mp4),
                width: w,
                height: h,
                framerate: 30.0,
                duration_micros: 1_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            common::model::ClipContent::Video(common::model::VideoContent {
                events: vec![common::model::VideoEvent {
                    source_id: vsrc_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    source_start_micros: 0,
                    source_end_micros: 1_000_000,
                    ..common::model::VideoEvent::default()
                }],
            }),
        );
        let track_id = song.alloc_track_id();
        let mut track = crate::app::track_with(|t| {
            t.id = track_id;
            t.name = "V".into();
        });
        let clip_id = track.alloc_clip_id();
        track.clips.push(common::model::Clip {
            id: clip_id,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        });
        song.tracks.push(track);
        song
    }

    /// End-to-end smoke: build a tiny project with one video track +
    /// one video clip pointing at an `ffmpeg`-generated source mp4,
    /// run `render_mp4`, and check the output exists + WMF can re-read
    /// it (= the container + H.264 stream were finalized correctly).
    /// Audio is skipped (video-only mp4) to keep the test fast and
    /// avoid pulling AAC encode setup into the smoke.
    #[test]
    fn render_mp4_video_only_smoke() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("render_mp4: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out_mp4 = dir.path().join("out.mp4");
        let src_mp4 = gen_blue_source(&ffmpeg, dir.path(), 320, 240);
        let song = build_one_video_song(src_mp4, 320, 240);

        let cfg = RenderConfig::new(&song, &out_mp4);
        let stats = match render_mp4(&cfg) {
            Ok(s) => s,
            Err(e) => {
                // The libav/NVENC backend needs the bundled FFmpeg DLLs + an
                // NVENC-capable GPU; both may be absent in CI. Skip rather than
                // fail when the encoder can't initialize.
                eprintln!("render_mp4_video_only_smoke: encoder unavailable ({e}); skipping");
                return;
            }
        };
        assert!(out_mp4.exists(), "output mp4 should exist");
        // 2 seconds @ 30fps = 60 frames (ceil rounding).
        assert!(
            stats.frames_written >= 58 && stats.frames_written <= 62,
            "frame count near 60, got {}",
            stats.frames_written
        );

        // Verify the file is a valid mp4 that WMF can open + read
        // metadata back from. Re-uses the existing extract_metadata
        // path so we get the width / height / codec round-trip in
        // one call.
        let md = crate::import_video::extract_metadata(&out_mp4)
            .expect("output mp4 should be readable by WMF");
        assert_eq!(md.width, 320);
        assert_eq!(md.height, 240);
        assert_eq!(md.codec, "h264");
    }

    /// r.md #32 end-to-end: 書き出し範囲が `length_beats` を超えても、render loop は
    /// **範囲全体** ぶんの frame を実 encode する。旧実装は end を `length_beats` に
    /// clamp して映像を半分に truncate していた (loop が length_beats を超える実
    /// プロジェクトで映像だけ途中で凍結し、audio と長さが割れた)。
    #[test]
    fn render_mp4_range_past_length_beats_renders_full_range() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("render_mp4: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out_mp4 = dir.path().join("out.mp4");
        let src_mp4 = gen_blue_source(&ffmpeg, dir.path(), 320, 240);
        // length_beats = 4 (= 2s @120bpm) の曲を、範囲 [0, 8] = 4s で書き出す。
        let song = build_one_video_song(src_mp4, 320, 240);
        assert_eq!(song.length_beats, 4.0, "fixture の length_beats は 4");

        let cfg = RenderConfig::new(&song, &out_mp4).with_range_beats(Some((0.0, 8.0)));
        let stats = match render_mp4(&cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("range smoke: encoder unavailable ({e}); skipping");
                return;
            }
        };
        // 8 beats @120bpm = 4s、@30fps = 120 frames。旧 clamp なら end=4 → 2s → 60
        // frames に truncate されていた。
        assert!(
            stats.frames_written >= 118 && stats.frames_written <= 122,
            "range [0,8] は length_beats(4) を超えても ~120 frames (4s) 出す (旧: 60), got {}",
            stats.frames_written
        );
    }

    /// the per-export output resolution + fps override must propagate
    /// all the way through the OffscreenRenderer + encoder into the actual mp4 —
    /// output dimensions equal the override (1280x720), NOT the project canvas
    /// (320x240), and the higher fps (60) yields ~double the frame count. This is
    /// the end-to-end proof that "書き出し時だけ指定" works (project untouched).
    #[test]
    fn render_mp4_honors_output_resolution_and_fps_override() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("render_mp4: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out_mp4 = dir.path().join("out.mp4");
        let src_mp4 = gen_blue_source(&ffmpeg, dir.path(), 320, 240);
        // Project canvas is 320x240 @ 30; the export overrides to 1280x720 @ 60.
        let song = build_one_video_song(src_mp4, 320, 240);

        let cfg = RenderConfig::new(&song, &out_mp4)
            .with_output_resolution(Some((1280, 720)))
            .with_output_framerate(Some(60.0));
        let stats = match render_mp4(&cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("override smoke: encoder unavailable ({e}); skipping");
                return;
            }
        };
        assert!(out_mp4.exists(), "output mp4 should exist");
        // 2 seconds @ 60fps = 120 frames (ceil rounding).
        assert!(
            stats.frames_written >= 118 && stats.frames_written <= 122,
            "frame count near 120 (60fps override), got {}",
            stats.frames_written
        );
        let md = crate::import_video::extract_metadata(&out_mp4)
            .expect("output mp4 should be readable by WMF");
        assert_eq!(md.width, 1280, "output width = override, not project canvas");
        assert_eq!(md.height, 720, "output height = override, not project canvas");
        assert_eq!(md.codec, "h264");
    }

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

}
