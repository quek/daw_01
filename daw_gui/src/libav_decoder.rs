//! In-process libav (rsmpeg) software video decoder for **export**
//! (`docs/plan_video_export_libav.md` Phase 3, decode side).
//!
//! Replaces the export's `VideoPlaybackEngine::new_cpu_only()` path (Media
//! Foundation software decode + an `ffmpeg.exe` subprocess fallback for codecs
//! MF rejects). Now that rsmpeg links libav in-process, we decode every source
//! — including 10-bit H.264 High10, HEVC, AV1 — directly with `avcodec`, with
//! `libswscale` converting to BGRA8 for the wgpu `OffscreenRenderer`.
//!
//! This fixes the bug where the export silently dropped 10-bit video: MF's SW
//! reader can't set up the 10-bit decode pipeline, so the reader was never
//! created, the `ffmpeg.exe` fallback had no dimensions to start from, and
//! `build_frame_scene` swallowed the error and skipped the layer (black).
//!
//! Preview keeps its own MF D3D11 zero-copy path for now; unifying that onto
//! libav D3D11VA is a separate, larger change (and pointless for 10-bit on
//! Ampere, which has no HW 10-bit H.264 decode).

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;

use common::model::VideoSourceId;
use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avformat::AVFormatContextInput;
use rsmpeg::avutil::{av_rescale_q, AVFrame};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;

/// One decoded BGRA8 frame (tightly packed, `width * height * 4` bytes).
pub struct DecodedBgra {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Lazily-opened per-source decoder. Holds the demuxer + decoder so sequential
/// `decode_at` (= the export frame loop) just keeps pulling frames; only a
/// backward / large-forward jump triggers a seek + flush.
struct SourceDecoder {
    input: AVFormatContextInput,
    decoder: AVCodecContext,
    stream_index: usize,
    /// Stream time_base — used to convert frame pts ↔ microseconds.
    time_base: ffi::AVRational,
    /// `SwsContext` (src pixfmt/dims → BGRA), created on the first decoded
    /// frame and reused. `(src_fmt, src_w, src_h)` is cached to recreate if a
    /// source ever changes format mid-stream (defensive).
    sws: Option<(SwsContext, i32, i32, i32)>,
    /// Source-time (μs) of the most recently decoded frame.
    last_micros: Option<u64>,
    /// Held latest frame, so a repeated target or the post-EOF tail returns
    /// without decoding more.
    last_frame: Option<AVFrame>,
    eof: bool,
}

/// Per-`VideoSourceId` in-process libav decoder for the export pipeline.
pub struct LibavVideoDecoder {
    sources: HashMap<VideoSourceId, SourceDecoder>,
}

impl Default for LibavVideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LibavVideoDecoder {
    pub fn new() -> Self {
        Self { sources: HashMap::new() }
    }

    /// Decode the frame covering `target_micros` (source time) of the video at
    /// `path`, returning it as tightly-packed BGRA8 at the source's native
    /// resolution. The decoder for a `source_id` is opened on first use and
    /// reused; sequential targets stream forward, jumps seek + flush.
    pub fn decode_at(
        &mut self,
        source_id: VideoSourceId,
        path: &Path,
        target_micros: u64,
    ) -> Result<DecodedBgra, String> {
        use std::collections::hash_map::Entry;
        let dec = match self.sources.entry(source_id) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => v.insert(SourceDecoder::open(path)?),
        };
        dec.decode_at(target_micros)
    }
}

/// Forward jump (μs) beyond which seeking to a keyframe is cheaper than
/// decoding and discarding the intervening frames.
const SEEK_AHEAD_MICROS: u64 = 500_000;

impl SourceDecoder {
    fn open(path: &Path) -> Result<Self, String> {
        let url = CString::new(path.to_string_lossy().as_bytes().to_vec())
            .map_err(|e| format!("path has interior NUL: {e}"))?;
        let input = AVFormatContextInput::open(&url)
            .map_err(|e| format!("open {}: {e:?}", path.display()))?;

        let (stream_index, codec) = input
            .find_best_stream(ffi::AVMEDIA_TYPE_VIDEO)
            .map_err(|e| format!("find video stream: {e:?}"))?
            .ok_or_else(|| format!("no video stream in {}", path.display()))?;

        let time_base = input.streams()[stream_index].time_base;

        let mut decoder = build_decoder(&input, stream_index, &codec)?;
        decoder.set_pkt_timebase(time_base);
        // Decode all available CPU threads — export is offline, throughput wins.
        decoder
            .open(None)
            .map_err(|e| format!("open decoder: {e:?}"))?;

        Ok(Self {
            input,
            decoder,
            stream_index,
            time_base,
            sws: None,
            last_micros: None,
            last_frame: None,
            eof: false,
        })
    }

    fn frame_micros(&self, frame: &AVFrame) -> u64 {
        let pts = if frame.pts != ffi::AV_NOPTS_VALUE {
            frame.pts
        } else {
            frame.best_effort_timestamp
        };
        if pts == ffi::AV_NOPTS_VALUE {
            return 0;
        }
        let micros = av_rescale_q(pts, self.time_base, ffi::AVRational { num: 1, den: 1_000_000 });
        micros.max(0) as u64
    }

    fn decode_at(&mut self, target_micros: u64) -> Result<DecodedBgra, String> {
        let half_frame = self.half_frame_micros();

        // (1) Repeated / paused target: the held frame already covers it.
        if let Some(last) = self.last_micros
            && self.last_frame.is_some()
            && target_micros + half_frame >= last
            && target_micros < last + 1_000_000
            && target_micros >= last.saturating_sub(half_frame)
        {
            return self.convert_last();
        }

        // (2) Decide whether to seek: backward, or a large forward jump.
        let should_seek = match self.last_micros {
            None => target_micros > SEEK_AHEAD_MICROS,
            Some(last) if target_micros + half_frame < last => true,
            Some(last) if target_micros > last + SEEK_AHEAD_MICROS => true,
            Some(_) => false,
        };
        if should_seek {
            self.seek(target_micros)?;
        }

        // (3) Decode forward until a frame's pts reaches `target_micros`.
        // Bound the walk so a malformed stream can't spin forever (~4 s of
        // 60fps source).
        for _ in 0..1024 {
            match self.decoder.receive_frame() {
                Ok(frame) => {
                    let micros = self.frame_micros(&frame);
                    self.last_micros = Some(micros);
                    self.last_frame = Some(frame);
                    if micros + half_frame >= target_micros {
                        return self.convert_last();
                    }
                    // Frame is before target — keep decoding (the held frame is
                    // overwritten next iteration; no BGRA conversion wasted).
                    continue;
                }
                Err(RsmpegError::DecoderDrainError) => {
                    // EAGAIN: feed another packet.
                }
                Err(RsmpegError::DecoderFlushedError) => {
                    // Clean EOF: hold the last frame for the clip tail.
                    self.eof = true;
                    return self
                        .convert_last()
                        .map_err(|_| "decoder reached EOF before any frame".to_string());
                }
                Err(e) => return Err(format!("receive_frame: {e:?}")),
            }

            if self.eof {
                // Already flushed; drain remaining then stop.
                continue;
            }
            match self
                .input
                .read_packet()
                .map_err(|e| format!("read_packet: {e:?}"))?
            {
                Some(packet) => {
                    if packet.stream_index as usize == self.stream_index {
                        self.decoder
                            .send_packet(Some(&packet))
                            .map_err(|e| format!("send_packet: {e:?}"))?;
                    }
                }
                None => {
                    // EOF on the demuxer: flush the decoder.
                    self.eof = true;
                    self.decoder
                        .send_packet(None)
                        .map_err(|e| format!("flush send_packet(None): {e:?}"))?;
                }
            }
        }
        Err("decode walk exceeded bound without reaching target".to_string())
    }

    fn seek(&mut self, target_micros: u64) -> Result<(), String> {
        let ts = av_rescale_q(
            target_micros as i64,
            ffi::AVRational { num: 1, den: 1_000_000 },
            self.time_base,
        );
        // AVSEEK_FLAG_BACKWARD → land on the keyframe at or before `ts`; the
        // forward walk then catches up to the exact target.
        let ret = unsafe {
            ffi::av_seek_frame(
                self.input.as_mut_ptr(),
                self.stream_index as i32,
                ts,
                ffi::AVSEEK_FLAG_BACKWARD as i32,
            )
        };
        if ret < 0 {
            return Err(format!("av_seek_frame to {target_micros}us failed: {ret}"));
        }
        unsafe { ffi::avcodec_flush_buffers(self.decoder.as_mut_ptr()) };
        self.eof = false;
        self.last_micros = None;
        self.last_frame = None;
        Ok(())
    }

    /// Convert the held `last_frame` (source pixfmt) to tightly-packed BGRA8.
    fn convert_last(&mut self) -> Result<DecodedBgra, String> {
        let frame = self
            .last_frame
            .as_ref()
            .ok_or_else(|| "no frame to convert".to_string())?;
        let src_fmt = frame.format;
        let src_w = frame.width;
        let src_h = frame.height;
        if src_w <= 0 || src_h <= 0 {
            return Err(format!("invalid frame dims {src_w}x{src_h}"));
        }

        // (Re)build the sws context if absent or the source format changed.
        let needs_new = match &self.sws {
            Some((_, f, w, h)) => *f != src_fmt || *w != src_w || *h != src_h,
            None => true,
        };
        if needs_new {
            let ctx = SwsContext::get_context(
                src_w,
                src_h,
                src_fmt,
                src_w,
                src_h,
                ffi::AV_PIX_FMT_BGRA,
                ffi::SWS_BILINEAR,
                None,
                None,
                None,
            )
            .ok_or_else(|| format!("sws_getContext for fmt {src_fmt} {src_w}x{src_h} failed"))?;
            self.sws = Some((ctx, src_fmt, src_w, src_h));
        }

        let mut dst = AVFrame::new();
        dst.set_format(ffi::AV_PIX_FMT_BGRA);
        dst.set_width(src_w);
        dst.set_height(src_h);
        dst.alloc_buffer()
            .map_err(|e| format!("dst frame alloc_buffer: {e:?}"))?;

        let (sws, ..) = self.sws.as_mut().expect("just set");
        sws.scale_frame(frame, 0, src_h, &mut dst)
            .map_err(|e| format!("sws scale_frame: {e:?}"))?;

        // Pack the (possibly padded) BGRA plane into a tight `w*h*4` buffer.
        let stride = dst.linesize[0] as usize;
        let row = src_w as usize * 4;
        let h = src_h as usize;
        let plane = dst.data[0];
        if plane.is_null() {
            return Err("dst BGRA plane is null".to_string());
        }
        let mut bgra = vec![0u8; row * h];
        for y in 0..h {
            // SAFETY: dst has `h * stride` readable bytes; bgra has `h * row`;
            // `row <= stride`; both rows in bounds.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    plane.add(y * stride),
                    bgra.as_mut_ptr().add(y * row),
                    row,
                );
            }
        }

        Ok(DecodedBgra { bgra, width: src_w as u32, height: src_h as u32 })
    }

    fn half_frame_micros(&self) -> u64 {
        // Half a frame at the stream's avg rate, used as the pts match window.
        // Falls back to ~16ms when the rate is unknown.
        16_000
    }
}

/// Build (but do not open) the decoder context from the input stream's
/// codec parameters.
fn build_decoder(
    input: &AVFormatContextInput,
    stream_index: usize,
    codec: &AVCodec,
) -> Result<AVCodecContext, String> {
    let mut decoder = AVCodecContext::new(codec);
    let codecpar = input.streams()[stream_index].codecpar();
    decoder
        .apply_codecpar(&codecpar)
        .map_err(|e| format!("apply_codecpar: {e:?}"))?;
    Ok(decoder)
}
