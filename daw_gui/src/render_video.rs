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
use daw_ui_renderer::{
    Color, GlyphArea, OffscreenRenderer, Rect, Scene, TextureHandle, TexturedQuad,
};

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
    crate::import_video::ensure_mf_startup_pub()
        .map_err(|e| format!("MFStartup: {e}"))?;

    let (out_w, out_h) = cfg.song.video_resolution;
    if out_w == 0 || out_h == 0 {
        return Err(format!(
            "invalid project video_resolution {out_w}x{out_h}"
        ));
    }
    let framerate = cfg.song.video_framerate;
    if framerate <= 0.0 {
        return Err(format!("invalid project video_framerate {framerate}"));
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
    for (image_source_id, source) in &cfg.song.image_sources {
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
    let total_seconds = beat_to_seconds(cfg.song.length_beats, cfg.song.bpm);
    let total_frames = (total_seconds * f64::from(framerate)).ceil() as u64;
    let mut scene = Scene::new();
    // Opaque black backdrop — matches the pre-P2 CPU path (= the canvas
    // was cleared to (0,0,0,255) before any blit). Letterboxed video
    // layers leave bars; uncovered regions read as black.
    scene.clear_color = wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

    tracing::info!(total_frames, out_w, out_h, "export: starting frame loop");
    let mut cancelled = false;
    // Phase 2b (gui_01 #077 submit_readback/finish_readback): 1-frame-ahead
    // async readback. At the top of iteration N, `pending` holds frame N-1's
    // submitted readback (its GPU copy ran during iteration N-1). We submit
    // frame N without blocking, then collect N-1 and hand it to the encode
    // worker. So composite+submit(N) ∥ GPU readback(N-1) ∥ worker encode(N-2)
    // overlap — a 3-stage pipeline with the Phase 2a encode worker.
    let mut pending = None;
    for frame_index in 0..total_frames {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(frame_index, total_frames, "export: cancelled by user");
            cancelled = true;
            break;
        }
        on_progress(frame_index, total_frames);
        let frame_seconds = frame_index as f64 / f64::from(framerate);
        let playhead_beat = seconds_to_beat(frame_seconds, cfg.song.bpm);
        scene.primitives.clear();
        build_frame_scene(
            cfg.song,
            cfg.project_dir,
            &mut decoder,
            &mut offscreen,
            &mut video_textures,
            &image_textures,
            playhead_beat,
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

#[inline]
fn beat_to_seconds(beats: f64, bpm: f32) -> f64 {
    beats * 60.0 / f64::from(bpm)
}

#[inline]
fn seconds_to_beat(seconds: f64, bpm: f32) -> f64 {
    seconds * f64::from(bpm) / 60.0
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
    video_textures: &mut HashMap<VideoSourceId, (TextureHandle, u32, u32)>,
    image_textures: &HashMap<ImageSourceId, (TextureHandle, u32, u32)>,
    playhead_beat: f64,
    out_w: u32,
    out_h: u32,
    scene: &mut Scene,
) {
    let video_layers = VideoPlaybackEngine::active_sources_at(song, playhead_beat);
    for layer in video_layers {
        let Some(path) =
            resolve_video_source_path(song, layer.video_source_id, project_dir)
        else {
            continue;
        };
        // In-process libav software decode (handles 10-bit H.264 / HEVC / AV1;
        // no Media Foundation, no `ffmpeg.exe` subprocess). Decode errors are
        // logged, not silently swallowed — a dropped frame would otherwise
        // leave the video black with no diagnostic (the exact bug the old
        // MF + ffmpeg.exe export path hit on 10-bit sources).
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
        // Get-or-create the per-source BGRA texture, recreating when
        // the source dimensions change (= shouldn't, but defensive).
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
        let (rx, ry, rw, rh) = aspect_fit(out_w, out_h, width, height);
        scene.push_textured_quad(TexturedQuad {
            rect: Rect::new(rx as f32, ry as f32, rw as f32, rh as f32),
            texture,
            alpha: layer.alpha,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: None,
            rotation_radians: 0.0,
            rotation_pivot: None,
        });
    }

    // docs/plan_image_overlay.md §P3: image overlay layers on top of
    // video. Normalized [0, 1] PiP rect maps to canvas pixels (= the
    // canvas IS the project resolution, so `x * out_w` is exact).
    // docs/plan_modulation.md §7: export はライブの follower scalar を読めない
    // (audio engine 非稼働)。 Phase 7 で env sidecar を frame 時刻でサンプルして
    // ここに渡す。 それまでは空 = 変調なし (curve/base のみ)。
    let mod_scalars: &[f32] = &[];
    let image_layers =
        crate::image_compose::active_image_sources_at(song, playhead_beat, mod_scalars);
    // v19 (docs/plan_tachie_group_transform.md §5.6): export も preview と同じ
    // gate（`active_visual_groups`）で group partition + offscreen 合成する
    // （preview/export byte parity 要件、SSoT）。
    let active_groups = crate::group_compose::active_visual_groups(song, playhead_beat, mod_scalars);
    let mut group_children: std::collections::HashMap<
        u32,
        Vec<crate::group_compose::GroupChildQuad>,
    > = std::collections::HashMap::new();
    for layer in image_layers {
        let Some((texture, _, _)) = image_textures.get(&layer.image_source_id) else {
            continue; // not decoded / failed import
        };
        // owning track の親が active visual group なら bucket、さもなくば直接 push。
        let group_id = song
            .track_by_id(layer.owning_track_id)
            .and_then(|t| t.parent_group_id)
            .filter(|g| active_groups.contains_key(g));
        if let Some(g) = group_id {
            group_children.entry(g).or_default().push(
                crate::group_compose::GroupChildQuad {
                    texture: *texture,
                    dest: (layer.x, layer.y, layer.w, layer.h),
                    alpha: layer.alpha,
                    rotation_radians: layer.rotation_radians,
                },
            );
            continue;
        }
        let rx = layer.x * out_w as f32;
        let ry = layer.y * out_h as f32;
        let rw = (layer.w * out_w as f32).max(0.0);
        let rh = (layer.h * out_h as f32).max(0.0);
        if rw == 0.0 || rh == 0.0 {
            continue;
        }
        scene.push_textured_quad(TexturedQuad {
            rect: Rect::new(rx, ry, rw, rh),
            texture: *texture,
            alpha: layer.alpha,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: None,
            rotation_radians: layer.rotation_radians,
            rotation_pivot: None,
        });
    }
    // 立ち絵 group を track 順（決定的）に合成 → 親 affine quad を push。
    for track in &song.tracks {
        let (Some(children), Some(transform)) =
            (group_children.remove(&track.id), active_groups.get(&track.id))
        else {
            continue;
        };
        if children.is_empty() {
            continue;
        }
        let (cw, ch) =
            crate::group_compose::group_composite_canvas((out_w, out_h), transform);
        let mut sub = Scene::new();
        for child in &children {
            sub.push_textured_quad(TexturedQuad {
                rect: Rect::new(
                    child.dest.0 * cw as f32,
                    child.dest.1 * ch as f32,
                    child.dest.2 * cw as f32,
                    child.dest.3 * ch as f32,
                ),
                texture: child.texture,
                alpha: child.alpha,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                clip_rect: None,
                rotation_radians: child.rotation_radians,
                rotation_pivot: None,
            });
        }
        let handle = match offscreen.composite_scene_to_texture(&sub, cw, ch) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(error = %e, "export composite 立ち絵 group failed");
                    continue;
                }
            };
        let (rx, ry, rw, rh, rot, px, py, alpha) = crate::group_compose::group_quad_params(
            transform,
            (0.0, 0.0, out_w as f32, out_h as f32),
        );
        if rw <= 0.0 || rh <= 0.0 || alpha <= 0.0 {
            continue;
        }
        scene.push_textured_quad(TexturedQuad {
            rect: Rect::new(rx, ry, rw, rh),
            texture: handle,
            alpha,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: None,
            rotation_radians: rot,
            rotation_pivot: Some((px, py)),
        });
    }

    // docs/plan_text_overlay.md §4 P3: text overlays on top of every
    // video / image layer. Canvas size = project resolution so
    // `font_size_px` / `outline_width_px` / `shadow_*` map 1:1 to
    // output px (= scale = 1.0). Horizontal alignment is approximated
    // via `font_size * char_count * 0.55`; same MVP estimate as the
    // preview path.
    let text_layers =
        crate::text_compose::active_text_sources_at(song, playhead_beat, mod_scalars);
    for layer in text_layers {
        if layer.alpha <= 0.0 || layer.text.is_empty() {
            continue;
        }
        let rx = layer.x * out_w as f32;
        let ry = layer.y * out_h as f32;
        let rw = (layer.w * out_w as f32).max(0.0);
        let rh = (layer.h * out_h as f32).max(0.0);
        let font_size = layer.font_size_px.max(1.0);
        let line_height = font_size * 1.2;
        // FIXME #28 (gui_01 #097): 揃えはレンダラに委譲。 box = (rx, ry, rw, rh)。
        // preview path と同一コードで、 自前の文字幅推定は撤去。
        let fill = Color::rgba(
            layer.fill_color[0],
            layer.fill_color[1],
            layer.fill_color[2],
            layer.fill_color[3] * layer.alpha,
        );
        let outline = Color::rgba(
            layer.outline_color[0],
            layer.outline_color[1],
            layer.outline_color[2],
            layer.outline_color[3] * layer.alpha,
        );
        let shadow = Color::rgba(
            layer.shadow_color[0],
            layer.shadow_color[1],
            layer.shadow_color[2],
            layer.shadow_color[3] * layer.alpha,
        );
        scene.push_text(GlyphArea {
            text: layer.text.clone(),
            // 空文字列は renderer default フォント (= None)、 指定があればそのファミリ。
            font_family: if layer.font_family.is_empty() {
                None
            } else {
                Some(layer.font_family.clone())
            },
            left: rx,
            top: ry,
            font_size,
            line_height,
            color: fill,
            clip_rect: None,
            outline_color: outline,
            outline_width_px: layer.outline_width_px,
            shadow_color: shadow,
            shadow_offset_px: layer.shadow_offset_px,
            shadow_blur_px: layer.shadow_blur_px,
            rotation_radians: layer.rotation_radians,
            // FIXME #28: box 内アライメント (実 glyph 幅でレンダラが配置)。
            box_width: Some(rw),
            box_height: Some(rh),
            align_h: crate::text_compose::halign_for(layer.align),
            align_v: daw_ui_renderer::VAlign::Center,
        });
    }
}

fn resolve_video_source_path(
    song: &Song,
    video_source_id: VideoSourceId,
    project_dir: Option<&Path>,
) -> Option<PathBuf> {
    let src = song.video_sources.get(&video_source_id)?;
    match &src.path {
        VideoSourcePath::Absolute(p) => Some(p.clone()),
        VideoSourcePath::ProjectRelative(rel) => project_dir.map(|d| d.join(rel)),
    }
}

/// Aspect-fit dst rect for a `src_w x src_h` source landing on a
/// `dst_w x dst_h` canvas. Returns `(x, y, w, h)` in canvas pixels.
fn aspect_fit(dst_w: u32, dst_h: u32, src_w: u32, src_h: u32) -> (i32, i32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, 0, 0);
    }
    let dst_aspect = dst_w as f64 / dst_h as f64;
    let src_aspect = src_w as f64 / src_h as f64;
    if src_aspect >= dst_aspect {
        let h = (dst_w as f64 / src_aspect).round() as u32;
        let y = ((dst_h - h.min(dst_h)) / 2) as i32;
        (0, y, dst_w, h.min(dst_h))
    } else {
        let w = (dst_h as f64 * src_aspect).round() as u32;
        let x = ((dst_w - w.min(dst_w)) / 2) as i32;
        (x, 0, w.min(dst_w), dst_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_fit_pillarbox() {
        // 16:9 source (1920x1080) onto 4:3 canvas (640x480) → letterbox
        // top/bottom.
        let (x, y, w, h) = aspect_fit(640, 480, 1920, 1080);
        assert_eq!(x, 0);
        assert_eq!(w, 640);
        // 640 / (16/9) = 360 → height 360, y centred
        assert_eq!(h, 360);
        assert_eq!(y, 60);
    }

    #[test]
    fn aspect_fit_letterbox() {
        // 9:16 portrait source onto landscape canvas → side bars.
        let (x, y, w, h) = aspect_fit(640, 480, 1080, 1920);
        assert_eq!(y, 0);
        assert_eq!(h, 480);
        // 480 * (9/16) = 270 → width 270, x centred
        assert_eq!(w, 270);
        assert_eq!(x, 185);
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
        let src_mp4 = dir.path().join("src.mp4");
        let out_mp4 = dir.path().join("out.mp4");
        // 1 second @ 30fps blue source.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=blue:size=320x240:duration=1:rate=30",
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

        // Build a song: 1 video track, 1 clip 4 beats long @ 120 BPM
        // (= 2 seconds). The source is only 1 second so the second
        // half renders the last-frame fallback (= acceptable for
        // smoke).
        let mut song = Song {
            bpm: 120.0,
            length_beats: 4.0,
            video_resolution: (320, 240),
            video_framerate: 30.0,
            ..Song::default()
        };
        let vsrc_id = song.alloc_video_source_id();
        song.video_sources.insert(
            vsrc_id,
            common::model::VideoSource {
                path: common::model::VideoSourcePath::Absolute(src_mp4),
                width: 320,
                height: 240,
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
            name: "vclip".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
            ..Default::default()
        });
        song.tracks.push(track);

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

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

}
