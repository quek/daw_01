//! Time a real video export through the new libav/NVENC backend
//! (`docs/plan_video_export_libav.md`). Loads a `.daw` project (JSON `Song`),
//! renders **video only** (no audio WAV) so the measurement isolates the
//! decode → composite → NVENC-encode path the user reported as slow, and
//! prints frames + wall time.
//!
//! ```pwsh
//! $env:PATH = "$PWD\third_party\ffmpeg\bin;$env:PATH"
//! cargo run -p daw_gui --example export_bench -- "<path>\to\project.daw"
//! ```

use std::path::Path;
use std::time::Instant;

use common::model::Song;
use daw_gui::render_video::{render_mp4, RenderConfig};

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: cargo run -p daw_gui --example export_bench -- <project.daw>");
        std::process::exit(2)
    };
    let daw = Path::new(&arg);
    let json = std::fs::read_to_string(daw).unwrap_or_else(|e| {
        eprintln!("read {}: {e}", daw.display());
        std::process::exit(1);
    });
    // .daw is a `{ "version": N, "song": {..} }` envelope.
    let envelope: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        eprintln!("parse .daw json from {}: {e}", daw.display());
        std::process::exit(1);
    });
    // (§10) 旧 .daw の legacy 構造を現行へ移し、untagged clip_contents に type タグを注入してから
    // deserialize (ファイル load 経路と同じ前処理)。
    let mut song_value = envelope["song"].clone();
    common::project::migrate_legacy_song(&mut song_value);
    common::project::tag_clip_contents_in_song(&mut song_value);
    let song: Song = serde_json::from_value(song_value).unwrap_or_else(|e| {
        eprintln!("parse Song from {} envelope: {e}", daw.display());
        std::process::exit(1);
    });

    let out = std::env::temp_dir().join("daw01_export_bench.mp4");
    // Synthetic PCM Float32 WAV (same format the audio engine produces) so the
    // bench exercises the full video + audio mux path, not just video.
    let secs = song.length_beats / song.bpm as f64 * 60.0;
    let sr = 48_000u32;
    let wav_path = std::env::temp_dir().join("daw01_export_bench_audio.wav");
    {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav_path, spec).expect("create wav");
        let total = (sr as f64 * secs) as usize;
        for n in 0..total {
            let s = (2.0 * std::f32::consts::PI * 440.0 * n as f32 / sr as f32).sin() * 0.2;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().expect("finalize wav");
    }

    let cfg = RenderConfig::new(&song, &out)
        .with_project_dir(daw.parent())
        .with_audio_wav(Some(&wav_path));

    println!(
        "exporting {}x{} @ {}fps, length {} beats @ {} bpm -> {}",
        song.video_resolution.0,
        song.video_resolution.1,
        song.video_framerate,
        song.length_beats,
        song.bpm,
        out.display()
    );

    let t = Instant::now();
    match render_mp4(&cfg) {
        Ok(stats) => {
            let secs = t.elapsed().as_secs_f64();
            let fps = stats.frames_written as f64 / secs.max(1e-6);
            println!(
                "OK: {} frames in {:.2}s ({:.1} fps encode throughput) -> {}",
                stats.frames_written,
                secs,
                fps,
                stats.output_path.display()
            );
        }
        Err(e) => {
            eprintln!("export failed after {:.2}s: {e}", t.elapsed().as_secs_f64());
            std::process::exit(1);
        }
    }
}
