//! Image file import (`docs/plan_image_overlay.md` P2).
//!
//! Pipeline (mirrors `import_audio.rs` / `import_video.rs`):
//!
//! 1. Compute SHA-256 prefix of the source file (8 hex chars) for
//!    content addressing.
//! 2. Copy the file into `<project_dir>/images/<basename>_<hash>.<ext>`
//!    (or, for unsaved projects, an `Absolute` cache path).
//! 3. Open with `image::open` → `DynamicImage::into_rgba8()` to get a
//!    width × height × 4-byte RGBA buffer (alpha-aware for transparent
//!    PNGs).
//! 4. Reorder RGBA → BGRA8 so the preview pipeline can hand the bytes
//!    straight to `Renderer::upload_texture_bgra` (= the same path
//!    video frames take, no shader changes needed).
//! 5. Build an `ImageSource` referencing the on-disk image path and
//!    return it alongside the decoded BGRA8 bytes (= caller uploads
//!    once into a per-source GPU `TextureHandle` cached on the preview
//!    window for the project's lifetime — static, no per-frame decode).
//!
//! Formats: PNG / JPEG / WebP (static) / BMP / TIFF / TGA / GIF
//! (static). The `image` crate's default features cover all of these.
//! Animated GIF / APNG / SVG / RAW are post-MVP.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use common::model::{ImageSource, ImageSourcePath};

use crate::import_audio::file_hash8;

#[derive(Debug)]
pub enum ImageImportError {
    /// File extension not in the supported set (= readable by image
    /// crate's default features). The caller filters drag&drop /
    /// dialog selection by extension before reaching here; this is
    /// a defensive last line.
    UnsupportedFormat(String),
    /// `image::open` failed (= corrupt file, unsupported subformat).
    DecodeFailed(String),
    /// File read / copy failed.
    IoError(String),
}

impl std::fmt::Display for ImageImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(ext) => write!(
                f,
                "Unsupported image format: .{ext} (P2 supports png/jpg/jpeg/webp/bmp/tiff/tga/gif)"
            ),
            Self::DecodeFailed(s) => write!(f, "Image decode failed: {s}"),
            Self::IoError(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for ImageImportError {}

/// Successful import outcome. The `bgra` buffer is meant to be uploaded
/// to a per-`ImageSourceId` GPU texture exactly once by the caller; the
/// `ImageSource` is registered in `Song.image_sources` so future
/// project loads can re-decode from disk.
#[derive(Debug)]
pub struct ImportedImage {
    pub source: ImageSource,
    /// Tightly-packed BGRA8 in scanline order, length =
    /// `source.width * source.height * 4`. Suitable for direct upload
    /// via `Renderer::upload_texture_bgra` (= the same path video
    /// frames take).
    pub bgra: Vec<u8>,
}

/// Sanitize a basename: keep ASCII alphanumerics, `_`, `-`. Anything
/// else → `_`. Empty stem (e.g. `.png`) becomes `image`.
fn sanitize_stem(stem: &str) -> String {
    let s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "image".into() } else { s }
}

/// Build the `<project_dir>/images/<basename>_<hash>.<ext>` filename
/// for a given source file. Mirrors `samples_filename` from
/// `import_audio` but lands in a sibling `images/` directory.
pub fn images_filename(src: &Path, hash8: &str) -> String {
    let stem = sanitize_stem(
        src.file_stem().and_then(|s| s.to_str()).unwrap_or("image"),
    );
    let ext = src
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png");
    format!("{stem}_{hash8}.{ext}")
}

/// Return `true` when the path's extension is a format `image` crate
/// can decode with its default features. Used by drag&drop / file
/// dialog filters and as a defensive guard inside `import_one_image`.
pub fn is_supported_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" | "tga" | "gif"
    )
}

/// Decode + cache + return the BGRA8 buffer for one image file.
///
/// `project_dir` is `Some` once the project has been saved; the file
/// is copied into a sibling `images/` subdir and the returned
/// `ImageSource.path` is `ProjectRelative`. When `project_dir` is
/// `None` (= unsaved new project), the cache lands at
/// `<temp>/daw_01/images/<basename>_<hash>.<ext>` and the path stored
/// is `Absolute` so the project remains loadable after save-as.
pub fn import_one_image(
    src: &Path,
    project_dir: Option<&Path>,
) -> Result<ImportedImage> {
    if !is_supported_extension(src) {
        let ext = src
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        return Err(anyhow!(ImageImportError::UnsupportedFormat(ext)));
    }
    if !src.exists() {
        return Err(anyhow!(ImageImportError::IoError(format!(
            "file not found: {}",
            src.display()
        ))));
    }

    let hash = file_hash8(src)
        .with_context(|| format!("hash {}", src.display()))?;
    let filename = images_filename(src, &hash);

    // Resolve the cache location. Saved projects get a project-relative
    // path; unsaved projects fall back to a temp cache under the
    // platform's temp dir (matches `import_audio` idiom).
    let (cache_path, path_variant) = match project_dir {
        Some(dir) => {
            let dest = dir.join("images").join(&filename);
            (dest, PathVariant::ProjectRelative)
        }
        None => {
            let mut tmp = std::env::temp_dir();
            tmp.push("daw_01");
            tmp.push("images");
            tmp.push(&filename);
            (tmp, PathVariant::Absolute)
        }
    };

    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("create dir {}", parent.display())
        })?;
    }

    if !cache_path.exists() {
        fs::copy(src, &cache_path).with_context(|| {
            format!(
                "copy {} → {}",
                src.display(),
                cache_path.display()
            )
        })?;
    }

    // Decode via image crate. `image::open` sniffs the format from the
    // first few bytes (= robust against renamed extensions). Errors
    // bubble through `ImageImportError::DecodeFailed`.
    let dynamic = image::open(&cache_path).map_err(|e| {
        anyhow!(ImageImportError::DecodeFailed(format!(
            "{}: {}",
            cache_path.display(),
            e
        )))
    })?;
    let format = image::ImageFormat::from_path(&cache_path)
        .map(|f| format!("{f:?}"))
        .unwrap_or_else(|_| "Unknown".to_string());

    let rgba = dynamic.into_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return Err(anyhow!(ImageImportError::DecodeFailed(format!(
            "zero-sized image: {}x{}",
            w, h
        ))));
    }

    // RGBA → BGRA8 reorder so the buffer drops into the preview
    // pipeline's `upload_texture_bgra` without any further conversion.
    // `into_raw()` consumes the RgbaImage's buffer in place (= no
    // extra allocation beyond the swap), and the result is the same
    // length we hand back to the caller.
    let mut bytes = rgba.into_raw();
    for px in bytes.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let stored_path = match path_variant {
        PathVariant::ProjectRelative => {
            // Strip project_dir prefix → relative path.
            let project_dir = project_dir.expect("project_dir present in this branch");
            let rel = cache_path
                .strip_prefix(project_dir)
                .map(PathBuf::from)
                .unwrap_or_else(|_| cache_path.clone());
            ImageSourcePath::ProjectRelative(rel)
        }
        PathVariant::Absolute => ImageSourcePath::Absolute(cache_path),
    };

    // 表示用に import 元ファイルの元名 (拡張子込み、 sanitize / hash 前) を
    // 保持する。 on-disk `path` は content addressing で sanitize / hash
    // 済みなので、 inspector / 口パク mapping ドロップダウンが元名を出すには
    // この `name` を SSoT にする (`ImageSource.name` doc 参照)。
    let original_name = src
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();

    Ok(ImportedImage {
        source: ImageSource {
            path: stored_path,
            name: original_name,
            width: w,
            height: h,
            format,
        },
        bgra: bytes,
    })
}

#[derive(Debug, Clone, Copy)]
enum PathVariant {
    ProjectRelative,
    Absolute,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Generate a tiny 2×2 PNG with known colors and verify the
    /// `import_one_image` BGRA buffer matches.
    #[test]
    fn import_one_image_decodes_png_and_emits_bgra() {
        // image crate can encode PNG natively without external deps.
        let dir = tempfile::tempdir().expect("tempdir");
        let png_path = dir.path().join("test.png");
        let mut img = image::RgbaImage::new(2, 2);
        // Top-left:    red (255, 0, 0, 255)
        // Top-right:   green (0, 255, 0, 255)
        // Bottom-left: blue (0, 0, 255, 255)
        // Bottom-right: semi-transparent yellow (255, 255, 0, 128)
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, image::Rgba([255, 255, 0, 128]));
        img.save(&png_path).expect("write png");

        let imported = import_one_image(&png_path, Some(dir.path()))
            .expect("import");

        assert_eq!(imported.source.width, 2);
        assert_eq!(imported.source.height, 2);
        assert_eq!(imported.source.format, "Png");
        assert_eq!(imported.bgra.len(), 2 * 2 * 4);

        // BGRA layout: byte order is B, G, R, A.
        // Top-left red → (0, 0, 255, 255)
        assert_eq!(&imported.bgra[0..4], &[0, 0, 255, 255]);
        // Top-right green → (0, 255, 0, 255)
        assert_eq!(&imported.bgra[4..8], &[0, 255, 0, 255]);
        // Bottom-left blue → (255, 0, 0, 255)
        assert_eq!(&imported.bgra[8..12], &[255, 0, 0, 255]);
        // Bottom-right semi-transparent yellow → (0, 255, 255, 128)
        assert_eq!(&imported.bgra[12..16], &[0, 255, 255, 128]);
    }

    #[test]
    fn import_one_image_preserves_original_japanese_name() {
        // 口パク mouth image を「あ.png」等の日本語名で import したとき、
        // on-disk path は sanitize で `_` に潰れるが、 表示用の
        // `source.name` には元名がそのまま残ること (= ドロップダウンで
        // 区別できる) を保証する。
        let dir = tempfile::tempdir().expect("tempdir");
        let png_path = dir.path().join("あ.png");
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        img.save(&png_path).expect("write png");

        let imported = import_one_image(&png_path, Some(dir.path())).expect("import");

        // name は元のファイル名 (拡張子込み、 sanitize 前) を保持。
        assert_eq!(imported.source.name, "あ.png");
        // on-disk path は sanitize で非 ASCII が `_` に潰れている。
        let stored = match &imported.source.path {
            ImageSourcePath::ProjectRelative(p) | ImageSourcePath::Absolute(p) => p,
        };
        let on_disk = stored.file_name().and_then(|s| s.to_str()).unwrap();
        assert!(
            !on_disk.contains('あ'),
            "on-disk filename must be sanitized: {on_disk}"
        );
        assert!(on_disk.starts_with('_'), "sanitized stem prefix: {on_disk}");
    }

    #[test]
    fn import_one_image_rejects_unsupported_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not_an_image.xyz");
        std::fs::File::create(&path)
            .expect("create")
            .write_all(b"not an image")
            .expect("write");
        let err = import_one_image(&path, Some(dir.path())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Unsupported image format"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn import_one_image_unsaved_project_uses_absolute_cache_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png_path = dir.path().join("logo.png");
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        img.save(&png_path).expect("write png");

        let imported = import_one_image(&png_path, None).expect("import");
        match imported.source.path {
            ImageSourcePath::Absolute(p) => {
                assert!(p.to_string_lossy().contains("daw_01"));
                assert!(p.to_string_lossy().contains("images"));
            }
            other => panic!("expected Absolute path, got {other:?}"),
        }
    }

    #[test]
    fn is_supported_extension_matches_documented_set() {
        assert!(is_supported_extension(Path::new("a.png")));
        assert!(is_supported_extension(Path::new("a.JPG")));
        assert!(is_supported_extension(Path::new("a.jpeg")));
        assert!(is_supported_extension(Path::new("a.webp")));
        assert!(is_supported_extension(Path::new("a.bmp")));
        assert!(is_supported_extension(Path::new("a.tif")));
        assert!(is_supported_extension(Path::new("a.tiff")));
        assert!(is_supported_extension(Path::new("a.tga")));
        assert!(is_supported_extension(Path::new("a.gif")));
        assert!(!is_supported_extension(Path::new("a.mp4")));
        assert!(!is_supported_extension(Path::new("a.txt")));
        assert!(!is_supported_extension(Path::new("noext")));
    }
}
