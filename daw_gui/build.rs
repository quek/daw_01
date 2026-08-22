// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! daw_gui build script — two responsibilities:
//!
//! 1. **App icon (#47):** rasterize `assets/icon.svg` (vector SSoT) at build
//!    time via resvg into a 256px straight-RGBA buffer (`OUT_DIR/window_icon.rgba`,
//!    consumed by `src/main.rs` for the winit window icon) and a multi-resolution
//!    `.ico` (16/32/48/256) embedded into the exe via embed-resource (Explorer /
//!    taskbar / Alt-Tab icon). See `docs/plan_icon_and_console.md`.
//!
//! 2. **FFmpeg DLLs:** copy the bundled shared DLLs (BtbN n7.1 lgpl-shared,
//!    under `third_party/ffmpeg/bin`) next to the built `daw_gui.exe` so the
//!    dynamically-linked rsmpeg / avcodec resolve them at runtime without any
//!    PATH munging. See `docs/plan_video_export_libav.md`.
//!    `FFMPEG_LIBS_DIR` is set by `.cargo/config.toml` to `third_party/ffmpeg/lib`;
//!    the DLLs live in the sibling `bin/`. We copy into the cargo target profile
//!    dir (the parent of `OUT_DIR`'s build tree), which is where `daw_gui.exe`
//!    lands.

use std::{env, fs, path::PathBuf};

use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use resvg::{tiny_skia, usvg};

/// daw_01 app icon source (256x256 square viewBox; no `<text>` so no font dep).
const ICON_SVG: &[u8] = include_bytes!("assets/icon.svg");
/// Sizes packed into the multi-res `.ico` (exe icon). `ico` stores 256 as PNG
/// and the smaller sizes as BMP automatically.
const ICO_SIZES: [u32; 4] = [16, 32, 48, 256];
/// winit window-icon edge size. Sole owner of the window-icon dimension —
/// `daw_gui/src/main.rs::window_icon()` derives the edge from the buffer length,
/// so there is no matching constant to keep in sync there.
const WINDOW_ICON_SIZE: u32 = 256;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_icon();
    copy_ffmpeg_dlls();
}

/// Rasterize the SVG to a square `size`x`size` **straight** (un-premultiplied)
/// RGBA8 buffer (`4*size*size` bytes, row-major, top-to-bottom, R,G,B,A).
/// tiny-skia pixels are premultiplied; `.ico` and winit want straight RGBA, so
/// demultiply via the built-in (exact at a==0/255, rounded for partial alpha).
fn rasterize(size: u32) -> Vec<u8> {
    let opt = usvg::Options::default(); // empty fontdb is fine: the icon has no text
    let tree = usvg::Tree::from_data(ICON_SVG, &opt).expect("parse assets/icon.svg");
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("alloc icon pixmap");
    let isz = tree.size();
    let transform =
        tiny_skia::Transform::from_scale(size as f32 / isz.width(), size as f32 / isz.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

/// #47: rasterize the icon -> `window_icon.rgba` (window icon) + multi-res `.ico`
/// embedded into the exe (Windows only).
fn build_icon() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    println!("cargo:rerun-if-changed=assets/icon.svg");

    // 1) winit window-icon RGBA buffer (main.rs `include_bytes!`s this).
    let win_rgba = rasterize(WINDOW_ICON_SIZE);
    fs::write(out_dir.join("window_icon.rgba"), &win_rgba).expect("write window_icon.rgba");

    // 2) multi-resolution .ico. One file carrying all sizes -> one ICON line ->
    //    Windows best-fits per context (taskbar 16/32, Explorer large 48/256).
    let mut dir = IconDir::new(ResourceType::Icon);
    for &edge in &ICO_SIZES {
        // Reuse the 256px buffer we already rasterized; render the rest.
        let rgba = if edge == WINDOW_ICON_SIZE {
            win_rgba.clone()
        } else {
            rasterize(edge)
        };
        debug_assert_eq!(rgba.len(), (4 * edge * edge) as usize);
        let image = IconImage::from_rgba_data(edge, edge, rgba); // takes ownership
        dir.add_entry(IconDirEntry::encode(&image).expect("encode .ico entry"));
    }
    let ico_path = out_dir.join("daw_gui.ico");
    let file = std::io::BufWriter::new(fs::File::create(&ico_path).expect("create daw_gui.ico"));
    dir.write(file).expect("write daw_gui.ico");

    // 3) embed the .ico into the exe so Explorer / taskbar / Alt-Tab show it.
    //    resource id 1 = lowest id = application/default icon. Windows only.
    #[cfg(windows)]
    {
        // rc string literals use backslash as an escape -> forward slashes.
        let ico_for_rc = ico_path.to_str().expect("ico path utf-8").replace('\\', "/");
        let rc_path = out_dir.join("daw_gui.rc");
        fs::write(&rc_path, format!("1 ICON \"{ico_for_rc}\"\n")).expect("write daw_gui.rc");
        // embed-resource does not emit rerun-if-changed itself.
        println!("cargo:rerun-if-changed={}", rc_path.display());
        embed_resource::compile(&rc_path, embed_resource::NONE)
            .manifest_optional()
            .expect("embed daw_gui icon resource");
    }
}

/// Copy the bundled FFmpeg shared DLLs next to the built binaries.
fn copy_ffmpeg_dlls() {
    println!("cargo:rerun-if-env-changed=FFMPEG_LIBS_DIR");

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
