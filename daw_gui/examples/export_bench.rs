//! Time a real video export through the new libav/NVENC backend
//! (`docs/plan_video_export_libav.md`). Loads a `.daw` project (JSON `Song`),
//! renders **video only** (no audio WAV) so the measurement isolates the
//! decode → composite → NVENC-encode path the user reported as slow, and
//! prints frames + wall time.
//!
//! ```pwsh
//! $env:PATH = "F:\dev\daw_01\third_party\ffmpeg\bin;$env:PATH"
//! cargo run -p daw_gui --example export_bench -- "C:\path\to\project.daw"
//! ```

use std::path::Path;
use std::time::Instant;

use common::model::Song;
use daw_gui::render_video::{render_mp4, RenderConfig};

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        r"C:\Users\ancient\Documents\daw_01\scratch\20260512\20260512.daw".to_string()
    });
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
    let song: Song = serde_json::from_value(envelope["song"].clone()).unwrap_or_else(|e| {
        eprintln!("parse Song from {} envelope: {e}", daw.display());
        std::process::exit(1);
    });

    let out = std::env::temp_dir().join("daw01_export_bench.mp4");
    let cfg = RenderConfig::new(&song, &out).with_project_dir(daw.parent());

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
