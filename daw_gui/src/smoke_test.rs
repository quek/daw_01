//! Visual regression smoke test for the video preview pipeline.
//!
//! # Why this exists
//!
//! Regression commit `c2ae697` (`chore(video): worker 側の dead keyed-mutex
//! Acquire/Release を削除`) passed every static check we had — `cargo build`,
//! `cargo test --workspace --lib`, `cargo clippy -- -D warnings`, runtime log
//! cleanliness — yet rendered the preview window as a fully transparent quad
//! over the dark backdrop. The bug was only visible by **looking at the
//! window**, and it took ~7 hours of human iteration to corner. This module
//! turns that visual check into a one-command assertion:
//!
//! ```text
//! cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4
//! # exit 0 = preview rendered visible content
//! # exit 1 = preview blank / uniform / transparent (regression)
//! ```
//!
//! The pattern would have caught `c2ae697` in seconds.
//!
//! # How it works
//!
//! 1. Caller spawns the orchestrator thread from `main.rs` **after** the
//!    runner has built `RunnerState`, passing the fixture path + the same
//!    `EventLoopProxy<AppEvent>` the runner consumes.
//! 2. The orchestrator drives a programmatic scenario via existing
//!    `AppEvent` variants (no new events introduced):
//!    `ImportVideo` → `TogglePreviewWindow` → `Play`.
//! 3. After ~1.5s of playback, it `FindWindowW`s the preview window by
//!    its title, `PrintWindow`s its client area into an HBITMAP, and
//!    `GetDIBits` to read the BGRA pixel data out.
//! 4. A histogram-based assertion (`validate_pixels`) flags blank /
//!    uniform / mostly-black captures.
//! 5. Pass → `std::process::exit(0)`. Fail → `tracing::error!` with the
//!    specific failure mode + `std::process::exit(1)`. We deliberately
//!    skip graceful shutdown — a smoke-test failure is a development
//!    signal, not a clean-exit path.
//!
//! # Why `PrintWindow` and not `BitBlt`
//!
//! Plain `BitBlt(GetWindowDC(hwnd), ...)` returns black for occluded /
//! DWM-composited windows on Windows 10/11. `PrintWindow` with the
//! `PW_RENDERFULLCONTENT` flag instructs the DWM compositor to re-render
//! the window into the destination DC, so the capture reflects the real
//! current pixel content even when the window is partially covered or
//! out of focus. Reference:
//! <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-printwindow>.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;

/// Preview window title hardcoded by [`crate::view::preview_window::PreviewWindowState::create`]
/// (`with_title("daw_01 — Video Preview")`). Kept in sync manually; the
/// runtime `FindWindowW` lookup uses this exact UTF-16-encoded string.
const PREVIEW_WINDOW_TITLE: &str = "daw_01 — Video Preview";

/// Histogram thresholds for [`validate_pixels`]. Calibrated against the
/// 320x240 `testsrc` fixture which renders as the SMPTE color-bar pattern
/// — a healthy preview shows dozens of unique colors and very few pure
/// black pixels. Tighter than necessary for testsrc so that subtler
/// future fixtures can reuse the same bar.
struct ValidationThresholds {
    /// Minimum distinct `(R, G, B)` tuples in the captured pixels.
    /// A blank / single-color preview produces 1-5; a transparent quad
    /// over a dark backdrop produces ~10 (just the backdrop colors).
    min_unique_colors: usize,
    /// Maximum percentage of pixels whose `R + G + B` sum is under 30
    /// (= near-black). A preview-window-only capture of the dark
    /// backdrop is >99% black; a real video frame is typically < 60%.
    max_black_percent: u32,
}

/// Calibrated against the `tests/fixtures/smoke_test.mp4` `testsrc`
/// pattern on Windows 11 / NVIDIA Vulkan: a healthy capture yields
/// **20 000+ unique colors** (color bars + anti-aliased borders +
/// JPEG-DCT-style fringes), a fully transparent / blank rendering
/// gives **< 20** (just the dark backdrop variants), and a *partially*
/// broken keyed-mutex protocol (= the `c2ae697`-class regression we
/// reproduced offline by short-circuiting the worker `Acquire/Release`
/// pair) gives **~130** — distinguishable from healthy by 100x but
/// not by the loose `< 50` line that would have let it slip through.
///
/// `min_unique_colors = 1000` sits comfortably in the gap (= 20x
/// margin against the worst degraded case, 20x margin under the
/// healthy floor). If a future fixture renders simpler content the
/// floor should be re-calibrated rather than dropped here.
const TESTSRC_THRESHOLDS: ValidationThresholds = ValidationThresholds {
    min_unique_colors: 1000,
    max_black_percent: 95,
};

/// docs/plan_text_overlay.md §4 P3: text overlay smoke thresholds.
/// シナリオは「AddTextClip 直後の default 「Title」 64 px white で
/// preview に描画されている」 こと。 期待値:
///
/// - **unique_colors ≥ 20**: white text の anti-aliased edges + dark
///   backdrop の glyphon offscreen composite 中間色で「色だけ」 で
///   ~50-300 個になる。 effects 無し / 有り 経路どちらでも clear。
///   20 を floor にしておけば「text 全く描画されていない」 (=
///   backdrop だけ ≈ 1-5 unique) を確実に検知。
/// - **max_black_percent ≤ 50**: preview backdrop は RGB sum ≈ 44 で
///   `< 30` の near-black 判定を外れる (= 0% black) はず。 万一 backdrop
///   が真っ黒に regression しても 50% 以下で fail させて検知。
const TEXT_OVERLAY_THRESHOLDS: ValidationThresholds = ValidationThresholds {
    min_unique_colors: 20,
    max_black_percent: 50,
};

/// Spawn the smoke-test orchestrator. Called from `main` when the
/// `--smoke-test <fixture>` flag is present, before the event loop
/// starts. The thread drives the scenario via `proxy.send_event(...)`
/// and exits the process directly with an explicit code.
///
/// The orchestrator never panics on missing windows / failed APIs —
/// every error path goes through the same exit(1) reporting so a CI
/// runner sees structured failure modes.
pub fn spawn_orchestrator(fixture: PathBuf, proxy: EventLoopProxy<AppEvent>) {
    if !fixture.exists() {
        eprintln!(
            "smoke-test: fixture not found at {} — cannot run",
            fixture.display()
        );
        std::process::exit(1);
    }

    let started = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));
    // Safety net: if any send_event or capture stalls, kill the process
    // after 30s instead of hanging the CI runner forever.
    spawn_watchdog(cancel.clone());

    thread::Builder::new()
        .name("smoke-test-orchestrator".to_string())
        .spawn(move || {
            tracing::info!(
                fixture = %fixture.display(),
                "smoke test orchestrator started"
            );

            // Phase 1: wait for daw_gui startup. main / preview Renderer
            // initialization takes ~700ms on a warm wgpu cache; budget
            // 1.2s for cold-cache + plugin scan kicking in.
            thread::sleep(Duration::from_millis(1200));

            // Phase 2: import the fixture as a video clip. AppData
            // handles `ImportVideo` synchronously on the GUI thread
            // (per its doc comment), so a single send is enough.
            if proxy
                .send_event(AppEvent::ImportVideo {
                    paths: vec![fixture.clone()],
                })
                .is_err()
            {
                fail("event loop closed before ImportVideo could be sent");
            }
            // Give the synchronous import time to run + first thumbnail
            // upload to finish. 2s is conservative for a 13KB fixture.
            thread::sleep(Duration::from_secs(2));

            // Phase 3: open the preview window. Sync pass on the next
            // frame's `sync_preview_window`.
            if proxy.send_event(AppEvent::TogglePreviewWindow).is_err() {
                fail("event loop closed before TogglePreviewWindow could be sent");
            }
            thread::sleep(Duration::from_millis(800));

            // Phase 4: start playback. Worker thread begins decoding
            // immediately; first frames arrive within a few hundred ms.
            if proxy.send_event(AppEvent::Play).is_err() {
                fail("event loop closed before Play could be sent");
            }
            // 1.5s of playback = ~45 frames at 30fps. Plenty of time
            // for the preview window to have rendered actual content.
            thread::sleep(Duration::from_millis(1500));

            // Phase 5: capture + validate.
            let elapsed = started.elapsed();
            tracing::info!(
                elapsed_ms = elapsed.as_millis() as u64,
                "capturing preview window"
            );
            let result = capture_and_validate(&TESTSRC_THRESHOLDS);

            cancel.store(true, Ordering::Release);
            match result {
                Ok(stats) => {
                    tracing::info!(
                        unique_colors = stats.unique_colors,
                        black_percent = stats.black_percent,
                        width = stats.width,
                        height = stats.height,
                        "smoke test PASSED"
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    tracing::error!(error = %e, "smoke test FAILED");
                    eprintln!("smoke test FAILED: {e}");
                    std::process::exit(1);
                }
            }
        })
        .expect("spawn smoke-test-orchestrator");
}

/// docs/plan_text_overlay.md §4: text overlay 用の smoke orchestrator。
/// 通常 smoke と違い fixture を取らず、 内部で `AddTextClip` を発火して
/// default "Title" clip を生成 → preview を開いて Play → 1.5s 後に
/// capture → `TEXT_OVERLAY_THRESHOLDS` で histogram 検証。 gui_01 Phase
/// 78 (GlyphArea text effects) landing 後の runtime regression 検知用。
pub fn spawn_text_overlay_orchestrator(proxy: EventLoopProxy<AppEvent>) {
    let started = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));
    spawn_watchdog(cancel.clone());

    thread::Builder::new()
        .name("smoke-test-text-orchestrator".to_string())
        .spawn(move || {
            tracing::info!("text overlay smoke test orchestrator started");

            // Phase 1: wait for daw_gui startup (= main window + wgpu
            // device init + plugin scan). 1.2s is conservative for cold
            // cache; warm-cache reruns clear in ~700ms.
            thread::sleep(Duration::from_millis(1200));

            // Phase 2: AddTextClip → 新 track + clip + 1 TextEvent
            // (text="Title" / 64px / 中央横帯 / shadow.a=0.5 で
            // gui_01 effects path も exercise する)。 handler は
            // GUI thread で同期実行 (= action_add_text_clip)。
            if proxy.send_event(AppEvent::AddTextClip).is_err() {
                fail("event loop closed before AddTextClip could be sent");
            }
            thread::sleep(Duration::from_millis(400));

            // Phase 3: preview window 開く。 sync_preview_window が次
            // frame で window 生成。
            if proxy.send_event(AppEvent::TogglePreviewWindow).is_err() {
                fail("event loop closed before TogglePreviewWindow could be sent");
            }
            thread::sleep(Duration::from_millis(800));

            // Phase 4: Play で playhead を進めて drive_preview_playback
            // が active_text_sources_at を resolve、 text を scene に
            // push する。 1.5s で十分 frame が積もる。
            if proxy.send_event(AppEvent::Play).is_err() {
                fail("event loop closed before Play could be sent");
            }
            thread::sleep(Duration::from_millis(1500));

            // Phase 5: capture + validate。
            let elapsed = started.elapsed();
            tracing::info!(
                elapsed_ms = elapsed.as_millis() as u64,
                "capturing preview window for text overlay smoke"
            );
            let result = capture_and_validate(&TEXT_OVERLAY_THRESHOLDS);

            cancel.store(true, Ordering::Release);
            match result {
                Ok(stats) => {
                    tracing::info!(
                        unique_colors = stats.unique_colors,
                        black_percent = stats.black_percent,
                        width = stats.width,
                        height = stats.height,
                        "text overlay smoke test PASSED"
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    tracing::error!(error = %e, "text overlay smoke test FAILED");
                    eprintln!("text overlay smoke test FAILED: {e}");
                    std::process::exit(1);
                }
            }
        })
        .expect("spawn smoke-test-text-orchestrator");
}

/// Watchdog: if the orchestrator doesn't `cancel.store(true)` within
/// 30s, force-exit. Prevents a stuck IPC / WMF / wgpu call from
/// hanging a CI runner indefinitely.
fn spawn_watchdog(cancel: Arc<AtomicBool>) {
    thread::Builder::new()
        .name("smoke-test-watchdog".to_string())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
            eprintln!("smoke test watchdog: orchestrator hung > 30s, killing process");
            std::process::exit(2);
        })
        .expect("spawn smoke-test-watchdog");
}

#[inline]
fn fail(reason: &str) -> ! {
    eprintln!("smoke test FAILED: {reason}");
    tracing::error!(error = reason, "smoke test FAILED");
    std::process::exit(1);
}

/// Capture statistics returned on a successful validation. Useful for
/// the pass-side log line so a CI run can spot histogram drift over
/// time without re-running the test.
#[derive(Debug)]
struct CaptureStats {
    unique_colors: usize,
    black_percent: u32,
    width: i32,
    height: i32,
}

fn capture_and_validate(thresholds: &ValidationThresholds) -> Result<CaptureStats, String> {
    let hwnd = find_preview_window()?;
    let capture = capture_window(hwnd)?;
    let stats = validate_pixels(&capture, thresholds)?;
    Ok(stats)
}

// ---------- Win32 capture ---------------------------------------------------

#[cfg(windows)]
fn find_preview_window() -> Result<windows::Win32::Foundation::HWND, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = std::ffi::OsStr::new(PREVIEW_WINDOW_TITLE)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `PCWSTR::from_raw(wide.as_ptr())` borrows the buffer for
    // the FindWindowW call only; `wide` lives through that call.
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR::from_raw(wide.as_ptr())) }
        .map_err(|e| format!("FindWindowW: {e}"))?;
    if hwnd.0.is_null() {
        return Err(format!(
            "preview window not found (looked for title {PREVIEW_WINDOW_TITLE:?})"
        ));
    }
    Ok(hwnd)
}

/// BGRA pixel buffer + dimensions. Capture is always top-down (= row 0
/// is the top of the window).
#[cfg(windows)]
struct CapturedFrame {
    width: i32,
    height: i32,
    /// Length = `width * height * 4` bytes, channel order **BGRA**
    /// (matches `BITMAPINFOHEADER::biBitCount = 32` default on
    /// little-endian Windows).
    bgra: Vec<u8>,
}

#[cfg(windows)]
fn capture_window(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<CapturedFrame, String> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }
        .map_err(|e| format!("GetClientRect: {e}"))?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(format!(
            "preview window has degenerate client area: {width}x{height}"
        ));
    }

    // GDI device contexts + bitmap. All wrapped in unsafe + manual
    // cleanup; using raw GDI here keeps the smoke test free of any
    // image-crate dependency.
    unsafe {
        let window_dc = GetDC(Some(hwnd));
        if window_dc.is_invalid() {
            return Err("GetDC returned null".to_string());
        }
        let mem_dc = CreateCompatibleDC(Some(window_dc));
        if mem_dc.is_invalid() {
            ReleaseDC(Some(hwnd), window_dc);
            return Err("CreateCompatibleDC failed".to_string());
        }
        let bitmap = CreateCompatibleBitmap(window_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(Some(hwnd), window_dc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }
        let prev_obj = SelectObject(mem_dc, bitmap.into());

        // PW_RENDERFULLCONTENT (= 0x00000002) asks DWM to re-render the
        // window into our DC even if it's hidden / occluded / on a
        // virtual desktop. Plain PrintWindow flags=0 was unreliable on
        // Windows 11 24H2 for wgpu surfaces.
        const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
        let pw_ok = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
        if !pw_ok.as_bool() {
            let _ = SelectObject(mem_dc, prev_obj);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem_dc);
            ReleaseDC(Some(hwnd), window_dc);
            return Err("PrintWindow returned false".to_string());
        }

        // BITMAPINFOHEADER with biHeight negative = top-down DIB. That
        // way row 0 of `bgra` is the top of the window — the obvious
        // memory layout.
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            ..Default::default()
        };
        let stride = (width as usize) * 4;
        let mut bgra = vec![0u8; stride * (height as usize)];
        let lines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );

        // Cleanup in reverse-allocation order, even on error.
        let _ = SelectObject(mem_dc, prev_obj);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(Some(hwnd), window_dc);

        if lines == 0 {
            return Err("GetDIBits returned 0 lines copied".to_string());
        }

        Ok(CapturedFrame {
            width,
            height,
            bgra,
        })
    }
}

#[cfg(not(windows))]
fn find_preview_window() -> Result<(), String> {
    Err("smoke test is Windows-only (Win32 capture path)".to_string())
}

#[cfg(not(windows))]
fn capture_window(_hwnd: ()) -> Result<CapturedFrame, String> {
    Err("smoke test is Windows-only (Win32 capture path)".to_string())
}

#[cfg(not(windows))]
struct CapturedFrame {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

// ---------- Pixel histogram validation --------------------------------------

fn validate_pixels(
    frame: &CapturedFrame,
    thresholds: &ValidationThresholds,
) -> Result<CaptureStats, String> {
    let total = (frame.width as usize) * (frame.height as usize);
    if total == 0 || frame.bgra.len() < total * 4 {
        return Err(format!(
            "captured frame is empty or undersized (width={} height={} bytes={})",
            frame.width,
            frame.height,
            frame.bgra.len()
        ));
    }

    // Use a fixed-size HashSet — we cap at min_unique_colors * 4 to
    // keep allocation bounded even on a healthy capture.
    let cap = thresholds.min_unique_colors.saturating_mul(4).max(256);
    let mut unique: std::collections::HashSet<(u8, u8, u8)> =
        std::collections::HashSet::with_capacity(cap);
    let mut black_count: usize = 0;
    for px in frame.bgra.chunks_exact(4) {
        // BGRA layout (Windows DIB top-down 32-bit).
        let b = px[0];
        let g = px[1];
        let r = px[2];
        unique.insert((r, g, b));
        if u32::from(r) + u32::from(g) + u32::from(b) < 30 {
            black_count += 1;
        }
    }

    let unique_colors = unique.len();
    let black_percent = ((black_count * 100) / total) as u32;

    if unique_colors < thresholds.min_unique_colors {
        return Err(format!(
            "captured preview has {unique_colors} unique colors (< {} required) — \
             likely uniform / blank rendering. Investigate the texture sampling path.",
            thresholds.min_unique_colors
        ));
    }
    if black_percent > thresholds.max_black_percent {
        return Err(format!(
            "captured preview is {black_percent}% black pixels (> {}% allowed) — \
             likely transparent quad over dark backdrop. Investigate the shared \
             texture import / keyed-mutex pair (see CLAUDE.md §FFI dead code).",
            thresholds.max_black_percent
        ));
    }

    Ok(CaptureStats {
        unique_colors,
        black_percent,
        width: frame.width,
        height: frame.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid-black capture must FAIL on the black-percent threshold.
    #[test]
    fn validate_rejects_all_black() {
        let frame = CapturedFrame {
            width: 4,
            height: 4,
            bgra: vec![0u8; 4 * 4 * 4],
        };
        let err = validate_pixels(&frame, &TESTSRC_THRESHOLDS)
            .expect_err("all-black must fail");
        assert!(
            err.contains("unique colors") || err.contains("black pixels"),
            "expected blank/black diagnostic, got: {err}"
        );
    }

    /// Two-color capture (= 50% black, 50% white) must FAIL on
    /// the unique-color threshold (only 2 < 50 required).
    #[test]
    fn validate_rejects_two_colors() {
        let pixel_count = 100;
        let mut bgra = Vec::with_capacity(pixel_count * 4);
        for i in 0..pixel_count {
            if i % 2 == 0 {
                bgra.extend_from_slice(&[0u8, 0, 0, 255]); // black
            } else {
                bgra.extend_from_slice(&[255u8, 255, 255, 255]); // white
            }
        }
        let frame = CapturedFrame {
            width: 10,
            height: 10,
            bgra,
        };
        let err = validate_pixels(&frame, &TESTSRC_THRESHOLDS)
            .expect_err("2-color must fail");
        assert!(
            err.contains("unique colors"),
            "expected unique-colors diagnostic, got: {err}"
        );
    }

    /// Synthetic densely-varied gradient must PASS — proves the
    /// validator doesn't reject legitimately varied output. Sized
    /// 64x64 = 4096 pixels with all-axis RGB variation so the
    /// unique-color count clears `TESTSRC_THRESHOLDS.min_unique_colors`
    /// (= 1000) comfortably.
    #[test]
    fn validate_accepts_color_gradient() {
        let mut bgra = Vec::with_capacity(64 * 64 * 4);
        for y in 0..64u8 {
            for x in 0..64u8 {
                let r = x * 4;
                let g = y * 4;
                let b = (x ^ y) * 4;
                bgra.extend_from_slice(&[b, g, r, 255]);
            }
        }
        let frame = CapturedFrame {
            width: 64,
            height: 64,
            bgra,
        };
        let stats =
            validate_pixels(&frame, &TESTSRC_THRESHOLDS).expect("gradient must pass");
        assert_eq!(stats.width, 64);
        assert_eq!(stats.height, 64);
        assert!(
            stats.unique_colors >= TESTSRC_THRESHOLDS.min_unique_colors,
            "gradient yielded {} unique colors, expected ≥ {}",
            stats.unique_colors,
            TESTSRC_THRESHOLDS.min_unique_colors
        );
    }

    /// Degenerate input (zero-sized frame) must be rejected with a
    /// clear diagnostic, not panic.
    #[test]
    fn validate_rejects_empty_frame() {
        let frame = CapturedFrame {
            width: 0,
            height: 0,
            bgra: Vec::new(),
        };
        let err = validate_pixels(&frame, &TESTSRC_THRESHOLDS)
            .expect_err("empty frame must fail");
        assert!(err.contains("empty") || err.contains("undersized"));
    }
}
