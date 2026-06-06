//! Copy the bundled FFmpeg shared DLLs (BtbN n7.1 lgpl-shared, under
//! `third_party/ffmpeg/bin`) next to the built `daw_gui.exe` so the
//! dynamically-linked rsmpeg / avcodec resolve them at runtime without any
//! PATH munging. See `docs/plan_video_export_libav.md`.
//!
//! `FFMPEG_LIBS_DIR` is set by `.cargo/config.toml` to `third_party/ffmpeg/lib`;
//! the DLLs live in the sibling `bin/`. We copy into the cargo target profile
//! dir (the parent of `OUT_DIR`'s build tree), which is where `daw_gui.exe`
//! lands.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_LIBS_DIR");
    println!("cargo:rerun-if-changed=build.rs");

    if !cfg!(windows) {
        return;
    }
    let Ok(libs_dir) = env::var("FFMPEG_LIBS_DIR") else {
        // Not configured (e.g. a check that doesn't link rsmpeg) — nothing to do.
        return;
    };
    let Some(bin) = PathBuf::from(&libs_dir).parent().map(|p| p.join("bin")) else {
        return;
    };
    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    // OUT_DIR = target/<profile>/build/daw_gui-XXXX/out → ancestors()[3] = target/<profile>.
    let Some(target_dir) = PathBuf::from(&out_dir).ancestors().nth(3).map(PathBuf::from) else {
        return;
    };

    // Windows resolves DLLs from the *loaded exe's* directory first. daw_gui.exe
    // lives in target/<profile>, but test binaries land in deps/ and examples in
    // examples/ — copy to all three so `cargo test` / `--example` exes load the
    // FFmpeg DLLs without any PATH munging.
    let dest_dirs = [
        target_dir.clone(),
        target_dir.join("deps"),
        target_dir.join("examples"),
    ];

    let Ok(entries) = fs::read_dir(&bin) else {
        println!("cargo:warning=FFmpeg bin dir not found: {}", bin.display());
        return;
    };
    let dlls: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("dll"))
        .collect();
    for dest_dir in &dest_dirs {
        let _ = fs::create_dir_all(dest_dir);
        for src in &dlls {
            let Some(name) = src.file_name() else { continue };
            let dest = dest_dir.join(name);
            // Copy only when missing or stale, so incremental builds stay cheap
            // (avcodec-61.dll alone is ~70 MB).
            let needs_copy = match (fs::metadata(src), fs::metadata(&dest)) {
                (Ok(s), Ok(d)) => match (s.modified(), d.modified()) {
                    (Ok(sm), Ok(dm)) => sm > dm,
                    _ => true,
                },
                _ => true,
            };
            if needs_copy && let Err(e) = fs::copy(src, &dest) {
                println!("cargo:warning=failed to copy {}: {e}", src.display());
            }
        }
    }
}
