// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! NVENC / libav export smoke (`docs/plan_video_export_libav.md`).
//!
//! Phase 1: verify encoder availability.
//! Phase 2: actually drive `LibavEncoder` — encode synthetic RGBA8 frames to an
//!          mp4 via h264_nvenc, with a known colour layout so the RGBA→NVENC
//!          channel order can be checked (top-left RED, top-right BLUE) and a
//!          moving GREEN bar so motion / P-frames are exercised.
//!
//! Run (FFmpeg bin is copied next to the exe by build.rs, but examples live in
//! a sibling dir, so add it to PATH):
//! ```pwsh
//! $env:PATH = "$PWD\third_party\ffmpeg\bin;$env:PATH"
//! cargo run -p daw_gui --example nvenc_smoke
//! ```
//! Then verify the printed mp4 with ffprobe + a corner-pixel colour check.

use std::ffi::CStr;

use daw_gui::libav_encoder::{AudioSpec, LibavEncoder};
use rsmpeg::avcodec::AVCodec;

fn report_encoder(name: &CStr) {
    match AVCodec::find_encoder_by_name(name) {
        Some(c) => println!(
            "  [ok]      encoder {:<13} -> {}",
            name.to_string_lossy(),
            c.long_name().to_string_lossy()
        ),
        None => println!("  [MISSING] encoder {}", name.to_string_lossy()),
    }
}

fn main() {
    println!("rsmpeg linked against FFmpeg — encoder availability:");
    report_encoder(c"h264_nvenc");
    report_encoder(c"aac");
    report_encoder(c"libopenh264");
    report_encoder(c"h264_mf");
    report_encoder(c"libx264");

    // What sw pixel formats does NVENC accept? Confirms RGBA-direct is valid.
    if let Some(codec) = AVCodec::find_encoder_by_name(c"h264_nvenc")
        && let Some(fmts) = codec.pix_fmts()
    {
        println!("  h264_nvenc pix_fmts (raw AVPixelFormat ids): {fmts:?}");
    }

    // Phase 2: encode a synthetic clip.
    let (w, h, fps, frames) = (1280u32, 720u32, 30.0f32, 60u32);
    let out_path = std::env::temp_dir().join("daw01_nvenc_smoke.mp4");
    println!("encoding {frames} frames {w}x{h}@{fps} -> {}", out_path.display());

    let (sr, ch) = (48_000u32, 2u32);
    let audio = Some(AudioSpec { sample_rate: sr, channels: ch, bitrate: 192_000 });
    let mut enc = match LibavEncoder::new(&out_path, w, h, fps, 5_000_000, audio) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("LibavEncoder::new failed: {e}");
            std::process::exit(1);
        }
    };

    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for f in 0..frames {
        fill_test_pattern(&mut rgba, w, h, f, frames);
        if let Err(e) = enc.push_video_rgba(&rgba) {
            eprintln!("push_video_rgba(frame {f}) failed: {e}");
            std::process::exit(1);
        }
    }

    // A 440 Hz stereo sine for the clip duration (frames / fps seconds).
    let secs = frames as f32 / fps;
    let total = (sr as f32 * secs) as usize;
    let mut pcm = Vec::with_capacity(total * ch as usize);
    for n in 0..total {
        let s = (2.0 * std::f32::consts::PI * 440.0 * n as f32 / sr as f32).sin() * 0.3;
        pcm.push(s);
        pcm.push(s);
    }
    if let Err(e) = enc.push_audio_interleaved(&pcm) {
        eprintln!("push_audio_interleaved failed: {e}");
        std::process::exit(1);
    }

    if let Err(e) = enc.finish() {
        eprintln!("finish failed: {e}");
        std::process::exit(1);
    }

    println!("nvenc_smoke: wrote {}", out_path.display());
    println!("  expect: top-left RED, top-right BLUE, a GREEN bar sweeping L->R");
}

/// Black background; RED 120x120 top-left, BLUE 120x120 top-right, a 40px GREEN
/// vertical bar sweeping left→right across the `frames`.
fn fill_test_pattern(rgba: &mut [u8], w: u32, h: u32, frame: u32, frames: u32) {
    let bar_x = ((frame as f32 / frames as f32) * (w as f32 - 40.0)) as u32;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let (r, g, b) = if y < 120 && x < 120 {
                (255, 0, 0) // RED top-left
            } else if y < 120 && x >= w - 120 {
                (0, 0, 255) // BLUE top-right
            } else if x >= bar_x && x < bar_x + 40 {
                (0, 255, 0) // GREEN sweeping bar
            } else {
                (16, 16, 16) // near-black bg
            };
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
}
