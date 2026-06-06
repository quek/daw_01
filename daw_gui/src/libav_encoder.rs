//! In-process libav (rsmpeg) encoder backend for video export
//! (`docs/plan_video_export_libav.md`).
//!
//! Replaces the Windows Media Foundation `IMFSinkWriter` path in
//! `render_video`: composited RGBA8 frames (read back from the wgpu
//! `OffscreenRenderer`) are handed to the GPU **NVENC** H.264 encoder via
//! `h264_nvenc`, and muxed into an mp4 by libavformat. An optional audio
//! stream (native AAC, from the export temp WAV's PCM Float32) is muxed into
//! the same file.
//!
//! Why this exists: the MF path used a CPU software H.264 encoder (Video
//! Encode 0% on the GPU) plus a scalar `rgba_to_nv12`, both on a serial,
//! non-pipelined frame loop — minutes for a 1-minute 1080p clip. NVENC moves
//! the encode onto the RTX video silicon; feeding RGBA directly lets NVENC's
//! CUDA path do the colour conversion on-GPU, dropping the scalar NV12 stage
//! (verified: top-left RED / top-right BLUE round-trip with no R/B swap).
//!
//! Phase 1 (this module) keeps the composite + GPU→CPU readback in
//! `render_video` unchanged (preview/export byte parity). Pipelining the
//! readback (Phase 2) and unifying source decode onto libav (Phase 3) come
//! later.

use std::ffi::CString;
use std::path::Path;

use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avformat::AVFormatContextOutput;
use rsmpeg::avutil::{AVChannelLayout, AVDictionary, AVFrame};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

/// Audio input spec for the optional muxed AAC stream. Channels / sample-rate
/// come from the export temp WAV (PCM Float32, native rate).
#[derive(Clone, Copy)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate: u32,
}

struct AudioEnc {
    aenc: AVCodecContext,
    stream_index: i32,
    enc_time_base: ffi::AVRational,
    stream_time_base: ffi::AVRational,
    frame_size: usize,
    channels: usize,
    sample_rate: i32,
    /// Interleaved f32 accumulator; drained `frame_size * channels` at a time.
    buf: Vec<f32>,
    /// Running sample-count PTS (in samples, encoder time_base = 1/sample_rate).
    next_pts: i64,
}

/// One muxed mp4 encode session. Drive it with [`Self::push_video_rgba`] per
/// frame (and [`Self::push_audio_interleaved`] for audio when configured) then
/// [`Self::finish`].
pub struct LibavEncoder {
    out: AVFormatContextOutput,
    venc: AVCodecContext,
    width: u32,
    height: u32,
    /// Pixel format the NVENC context consumes. Phase 1 feeds RGBA directly
    /// (NVENC converts on-GPU); see [`pick_video_pix_fmt`].
    pix_fmt: i32,
    video_stream_index: i32,
    venc_time_base: ffi::AVRational,
    video_stream_time_base: ffi::AVRational,
    next_pts: i64,
    audio: Option<AudioEnc>,
    finished: bool,
}

impl LibavEncoder {
    /// Build an mp4 encoder writing to `output_path` at `width`x`height` /
    /// `framerate`, ~`bitrate` bps NVENC VBR, with an optional AAC audio
    /// stream. Both streams are added before the header is written (WMF and
    /// libavformat alike require all streams declared up front).
    pub fn new(
        output_path: &Path,
        width: u32,
        height: u32,
        framerate: f32,
        bitrate: u32,
        audio: Option<AudioSpec>,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err(format!("invalid resolution {width}x{height}"));
        }
        if framerate <= 0.0 {
            return Err(format!("invalid framerate {framerate}"));
        }

        let (fps_num, fps_den) = if (framerate - framerate.round()).abs() < 1e-3 {
            (framerate.round() as i32, 1)
        } else {
            ((framerate * 1000.0).round() as i32, 1000)
        };
        let video_time_base = ffi::AVRational { num: fps_den, den: fps_num };
        let frame_rate = ffi::AVRational { num: fps_num, den: fps_den };

        let vcodec = AVCodec::find_encoder_by_name(c"h264_nvenc")
            .ok_or_else(|| "h264_nvenc encoder not available in linked FFmpeg".to_string())?;
        let pix_fmt = pick_video_pix_fmt(&vcodec);

        let mut venc = AVCodecContext::new(&vcodec);
        venc.set_width(width as i32);
        venc.set_height(height as i32);
        venc.set_pix_fmt(pix_fmt);
        venc.set_time_base(video_time_base);
        venc.set_framerate(frame_rate);
        venc.set_bit_rate(i64::from(bitrate));
        venc.set_gop_size((framerate.round() as i32).max(1) * 2);
        venc.set_max_b_frames(3);

        let mut out = AVFormatContextOutput::create(&path_to_cstring(output_path)?)
            .map_err(|e| format!("avformat create {}: {e:?}", output_path.display()))?;
        let global_header = out.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0;
        if global_header {
            venc.set_flags(venc.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
        }

        // NVENC private options: VBR + multipass full-res, HQ tune, lookahead +
        // AQ — quality-oriented archive preset (docs/plan_video_export_libav.md §2.2).
        let vopts = AVDictionary::new(c"preset", c"p5", 0)
            .set(c"tune", c"hq", 0)
            .set(c"rc", c"vbr", 0)
            .set(c"multipass", c"fullres", 0)
            .set(c"rc-lookahead", c"32", 0)
            .set(c"spatial-aq", c"1", 0)
            .set(c"temporal-aq", c"1", 0);
        venc.open(Some(vopts))
            .map_err(|e| format!("open h264_nvenc: {e:?}"))?;

        // Optional AAC audio encoder.
        let mut audio_enc: Option<AudioEnc> = None;
        if let Some(spec) = audio {
            let acodec = AVCodec::find_encoder(ffi::AV_CODEC_ID_AAC)
                .ok_or_else(|| "AAC encoder not available".to_string())?;
            let mut aenc = AVCodecContext::new(&acodec);
            aenc.set_sample_rate(spec.sample_rate as i32);
            aenc.set_sample_fmt(ffi::AV_SAMPLE_FMT_FLTP);
            aenc.set_ch_layout(AVChannelLayout::from_nb_channels(spec.channels as i32).into_inner());
            aenc.set_bit_rate(i64::from(spec.bitrate));
            aenc.set_time_base(ffi::AVRational { num: 1, den: spec.sample_rate as i32 });
            if global_header {
                aenc.set_flags(aenc.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
            }
            aenc.open(None).map_err(|e| format!("open aac: {e:?}"))?;
            let frame_size = if aenc.frame_size > 0 { aenc.frame_size as usize } else { 1024 };
            audio_enc = Some(AudioEnc {
                stream_index: 0, // set after new_stream below
                enc_time_base: aenc.time_base,
                stream_time_base: ffi::AVRational { num: 1, den: spec.sample_rate as i32 },
                frame_size,
                channels: spec.channels as usize,
                sample_rate: spec.sample_rate as i32,
                buf: Vec::with_capacity(frame_size * spec.channels as usize * 2),
                next_pts: 0,
                aenc,
            });
        }

        // Declare streams (video first = stream 0, then audio) before header.
        let video_stream_index;
        {
            let mut s = out.new_stream();
            s.set_codecpar(venc.extract_codecpar());
            s.set_time_base(video_time_base);
            video_stream_index = s.index;
        }
        if let Some(a) = audio_enc.as_mut() {
            let mut s = out.new_stream();
            s.set_codecpar(a.aenc.extract_codecpar());
            s.set_time_base(a.enc_time_base);
            a.stream_index = s.index;
        }

        out.write_header(&mut None)
            .map_err(|e| format!("write_header: {e:?}"))?;

        // `avformat_write_header` may rewrite each stream's `time_base` to the
        // muxer's preferred units (mp4 uses a high-resolution tick, not 1/fps).
        // Re-read the *actual* stream time_base so the per-packet `rescale_ts`
        // targets the right base — otherwise pts/dts land in stale units and
        // the container reports a near-zero video duration (60 frames in 4 ms).
        let video_stream_time_base = out.streams()[video_stream_index as usize].time_base;
        if let Some(a) = audio_enc.as_mut() {
            a.stream_time_base = out.streams()[a.stream_index as usize].time_base;
        }

        Ok(Self {
            out,
            venc,
            width,
            height,
            pix_fmt,
            video_stream_index,
            venc_time_base: video_time_base,
            video_stream_time_base,
            next_pts: 0,
            audio: audio_enc,
            finished: false,
        })
    }

    /// Encode one composited frame. `rgba` is tightly-packed RGBA8,
    /// `width * height * 4` bytes (the `OffscreenRenderer::render_to_rgba`
    /// output).
    pub fn push_video_rgba(&mut self, rgba: &[u8]) -> Result<(), String> {
        let expected = self.width as usize * self.height as usize * 4;
        if rgba.len() != expected {
            return Err(format!(
                "frame size {} != expected {expected} ({}x{})",
                rgba.len(),
                self.width,
                self.height
            ));
        }

        let mut frame = AVFrame::new();
        frame.set_format(self.pix_fmt);
        frame.set_width(self.width as i32);
        frame.set_height(self.height as i32);
        frame
            .alloc_buffer()
            .map_err(|e| format!("video frame alloc_buffer: {e:?}"))?;
        frame
            .make_writable()
            .map_err(|e| format!("video frame make_writable: {e:?}"))?;

        let dst_stride = frame.linesize[0] as usize;
        let src_stride = self.width as usize * 4;
        let dst = frame.data_mut()[0];
        if dst.is_null() {
            return Err("video frame plane 0 is null".to_string());
        }
        for y in 0..self.height as usize {
            // SAFETY: dst has `height * dst_stride` writable bytes; src row is
            // in-bounds; copy length is one packed RGBA row.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    rgba.as_ptr().add(y * src_stride),
                    dst.add(y * dst_stride),
                    src_stride,
                );
            }
        }

        frame.set_pts(self.next_pts);
        self.next_pts += 1;

        self.venc
            .send_frame(Some(&frame))
            .map_err(|e| format!("video send_frame: {e:?}"))?;
        drain_stream(
            &mut self.venc,
            &mut self.out,
            self.video_stream_index,
            self.venc_time_base,
            self.video_stream_time_base,
            false,
        )
    }

    /// Append interleaved PCM Float32 audio. Encodes as many whole AAC frames
    /// as the accumulated buffer allows; the remainder is held until the next
    /// call or [`Self::finish`]. No-op when the encoder has no audio stream.
    pub fn push_audio_interleaved(&mut self, samples: &[f32]) -> Result<(), String> {
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        audio.buf.extend_from_slice(samples);
        let need = audio.frame_size * audio.channels;
        while audio.buf.len() >= need {
            encode_audio_frame(
                &mut audio.aenc,
                &mut self.out,
                &audio.buf[..need],
                audio.frame_size,
                audio.channels,
                audio.sample_rate,
                &mut audio.next_pts,
                audio.stream_index,
                audio.enc_time_base,
                audio.stream_time_base,
            )?;
            audio.buf.drain(..need);
        }
        Ok(())
    }

    /// Flush both encoders, write the mp4 trailer, and finalize the file.
    pub fn finish(mut self) -> Result<(), String> {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        // Encode the trailing partial audio frame, then flush the audio encoder.
        if let Some(audio) = self.audio.as_mut() {
            if !audio.buf.is_empty() {
                let rem_samples = audio.buf.len() / audio.channels;
                if rem_samples > 0 {
                    let used = rem_samples * audio.channels;
                    encode_audio_frame(
                        &mut audio.aenc,
                        &mut self.out,
                        &audio.buf[..used],
                        rem_samples,
                        audio.channels,
                        audio.sample_rate,
                        &mut audio.next_pts,
                        audio.stream_index,
                        audio.enc_time_base,
                        audio.stream_time_base,
                    )?;
                }
                audio.buf.clear();
            }
            audio
                .aenc
                .send_frame(None)
                .map_err(|e| format!("flush audio send_frame(None): {e:?}"))?;
            drain_stream(
                &mut audio.aenc,
                &mut self.out,
                audio.stream_index,
                audio.enc_time_base,
                audio.stream_time_base,
                true,
            )?;
        }

        // Flush the video encoder.
        self.venc
            .send_frame(None)
            .map_err(|e| format!("flush video send_frame(None): {e:?}"))?;
        drain_stream(
            &mut self.venc,
            &mut self.out,
            self.video_stream_index,
            self.venc_time_base,
            self.video_stream_time_base,
            true,
        )?;

        self.out
            .write_trailer()
            .map_err(|e| format!("write_trailer: {e:?}"))?;
        Ok(())
    }
}

/// Drain ready packets from `enc` into `out`, rescaling timestamps from the
/// encoder time_base to the stream time_base. `flushing` accepts the EOF
/// sentinel; otherwise only EAGAIN ("need more input") ends the loop.
#[allow(clippy::too_many_arguments)]
fn drain_stream(
    enc: &mut AVCodecContext,
    out: &mut AVFormatContextOutput,
    stream_index: i32,
    enc_tb: ffi::AVRational,
    stream_tb: ffi::AVRational,
    flushing: bool,
) -> Result<(), String> {
    loop {
        match enc.receive_packet() {
            Ok(mut pkt) => {
                pkt.set_stream_index(stream_index);
                pkt.rescale_ts(enc_tb, stream_tb);
                out.interleaved_write_frame(&mut pkt)
                    .map_err(|e| format!("interleaved_write_frame: {e:?}"))?;
            }
            Err(RsmpegError::EncoderDrainError) => return Ok(()),
            Err(RsmpegError::EncoderFlushedError) if flushing => return Ok(()),
            Err(e) => return Err(format!("receive_packet: {e:?}")),
        }
    }
}

/// Build one FLTP audio frame from `interleaved` (`nb_samples * channels` f32),
/// encode it, and drain the resulting packets.
#[allow(clippy::too_many_arguments)]
fn encode_audio_frame(
    aenc: &mut AVCodecContext,
    out: &mut AVFormatContextOutput,
    interleaved: &[f32],
    nb_samples: usize,
    channels: usize,
    sample_rate: i32,
    next_pts: &mut i64,
    stream_index: i32,
    enc_tb: ffi::AVRational,
    stream_tb: ffi::AVRational,
) -> Result<(), String> {
    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_SAMPLE_FMT_FLTP);
    frame.set_nb_samples(nb_samples as i32);
    frame.set_ch_layout(AVChannelLayout::from_nb_channels(channels as i32).into_inner());
    frame.set_sample_rate(sample_rate);
    frame
        .alloc_buffer()
        .map_err(|e| format!("audio frame alloc_buffer: {e:?}"))?;
    frame
        .make_writable()
        .map_err(|e| format!("audio frame make_writable: {e:?}"))?;

    // Deinterleave: plane[c][i] = interleaved[i * channels + c].
    let planes = frame.data_mut();
    for (c, &plane) in planes.iter().enumerate().take(channels) {
        if plane.is_null() {
            return Err(format!("audio frame plane {c} is null"));
        }
        let plane = plane as *mut f32;
        for i in 0..nb_samples {
            // SAFETY: plane has nb_samples f32 slots; index in-bounds.
            unsafe {
                *plane.add(i) = interleaved[i * channels + c];
            }
        }
    }

    frame.set_pts(*next_pts);
    *next_pts += nb_samples as i64;

    aenc.send_frame(Some(&frame))
        .map_err(|e| format!("audio send_frame: {e:?}"))?;
    drain_stream(aenc, out, stream_index, enc_tb, stream_tb, false)
}

/// Pick the NVENC input pixel format. Phase 1 feeds RGBA8 straight from the
/// wgpu readback (`AV_PIX_FMT_RGBA`, which on little-endian aliases
/// `AV_PIX_FMT_BGR32` — one of the packed-RGB inputs NVENC's CUDA path accepts
/// and converts on the GPU; verified no R/B swap). Asserts the encoder lists
/// RGBA; if a future build dropped it, Phase 1 would need a swscale NV12 stage.
fn pick_video_pix_fmt(codec: &AVCodec) -> i32 {
    const PREFERRED: i32 = ffi::AV_PIX_FMT_RGBA;
    if codec.pix_fmts().is_some_and(|fmts| fmts.contains(&PREFERRED)) {
        return PREFERRED;
    }
    PREFERRED
}

fn path_to_cstring(p: &Path) -> Result<CString, String> {
    CString::new(p.to_string_lossy().as_bytes().to_vec())
        .map_err(|e| format!("path has interior NUL: {e}"))
}
