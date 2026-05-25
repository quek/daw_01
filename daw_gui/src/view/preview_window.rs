//! Video preview window (`docs/plan_video.md` P4).
//!
//! Second top-level winit window dedicated to displaying the project's
//! video output at the current playhead. P4 ships the window
//! infrastructure only — a dark background plus a "Video Preview"
//! placeholder text. P5 (lookahead decode + sync) and P7 (multi-track
//! composite) fill in the actual frame content via wgpu textured
//! quads.
//!
//! Lifecycle is driven by `AppData.preview_window_visible`:
//!
//! - `false` → no preview window exists (= the field starts false so
//!   daw_gui boots without one)
//! - `true` → the runner creates a `PreviewWindowState` on the next
//!   frame, including a fresh `winit::Window`, a `Renderer` over a
//!   `DawGuiWindow` wrapper, and a `Scene`
//! - User clicks the window's close button → the runner notices the
//!   `WindowEvent::CloseRequested` and flips the field back to false,
//!   which destroys the state on the next frame

use std::sync::Arc;

use daw_ui_platform::WindowBackend;
use daw_ui_renderer::{Color, GlyphArea, Renderer, Scene, TextureHandle, TexturedQuad};
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::{WindowAttributes, WindowId};

use crate::view::window::DawGuiWindow;

/// Per-window state owned by the runner while the preview is visible.
/// Dropped (= destroys the OS window) when `AppData.preview_window_visible`
/// transitions back to `false`.
pub struct PreviewWindowState {
    pub window: Arc<DawGuiWindow>,
    pub renderer: Renderer<DawGuiWindow>,
    pub scene: Scene,
    /// docs/plan_video.md P5: the texture the runner re-uploads each
    /// frame with the current playhead's decoded video. Created
    /// lazily on first decode (so the placeholder still renders
    /// before any video clip is in scope) and reused for every
    /// subsequent frame to avoid GPU allocator churn at 30fps. The
    /// `(width, height)` is the source's native size; the preview
    /// window may differ and `render_frame` aspect-fits into its own
    /// dimensions.
    pub frame_texture: Option<TextureHandle>,
    pub frame_size: (u32, u32),
}

impl PreviewWindowState {
    /// Create the OS window + wgpu Renderer. `initial_size` is taken
    /// from `Song.video_resolution` scaled to fit on common 1080p
    /// monitors (the project may be 4K but the preview window
    /// shouldn't bigger than ~half the screen by default; user resize
    /// is allowed and the wgpu surface tracks it).
    pub fn create(
        event_loop: &ActiveEventLoop,
        initial_size: (u32, u32),
    ) -> Result<Self, String> {
        let (w, h) = scale_to_fit_on_screen(initial_size);
        let attrs = WindowAttributes::default()
            .with_title("daw_01 — Video Preview")
            .with_inner_size(LogicalSize::new(w, h));
        let window = event_loop
            .create_window(attrs)
            .map_err(|e| format!("create preview window: {e}"))?;
        let window = Arc::new(window);
        let dwin = Arc::new(DawGuiWindow::new(window));
        let renderer = Renderer::new(dwin.clone())
            .map_err(|e| format!("preview Renderer::new: {e}"))?;
        Ok(Self {
            window: dwin,
            renderer,
            scene: Scene::new(),
            frame_texture: None,
            frame_size: (0, 0),
        })
    }

    /// Upload (or create + upload) the latest decoded video frame into
    /// the preview window's dedicated texture. The handle is reused
    /// every frame; the texture is recreated only when the source's
    /// dimensions change (= switching between video clips with
    /// different resolutions).
    pub fn upload_frame(&mut self, width: u32, height: u32, rgba: &[u8]) {
        if self.frame_size != (width, height) {
            if let Some(old) = self.frame_texture.take() {
                self.renderer.destroy_texture(old);
            }
            self.frame_texture = Some(self.renderer.create_texture(width, height));
            self.frame_size = (width, height);
        }
        if let Some(h) = self.frame_texture {
            self.renderer.upload_texture_rgba(h, rgba);
        }
    }

    /// Drop the cached video frame texture (= preview falls back to
    /// the placeholder until the next `upload_frame`). Called when the
    /// playhead leaves all video clips so a stale frame doesn't linger.
    pub fn clear_frame(&mut self) {
        if let Some(old) = self.frame_texture.take() {
            self.renderer.destroy_texture(old);
        }
        self.frame_size = (0, 0);
    }

    /// `winit::WindowId` for routing `WindowEvent`s in the runner.
    pub fn window_id(&self) -> WindowId {
        self.window.inner().id()
    }

    /// Resize handler — keep the wgpu surface and the cached size in
    /// sync. `daw_ui_platform::PhysicalSize` mirrors the winit one,
    /// just decoupled from the platform crate.
    pub fn resize(&mut self, size: daw_ui_platform::PhysicalSize) {
        self.renderer.resize(size);
        self.window.request_redraw();
    }

    /// Build the scene + render. When `frame_texture` is `Some` (=
    /// the runner has uploaded a decoded video frame for this
    /// playhead position, P5) the scene draws an aspect-fit textured
    /// quad over the dark backdrop; otherwise it falls back to the
    /// "Video Preview" placeholder text (P4 baseline).
    pub fn render_placeholder(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        // Dark backdrop spanning the entire window so any unfilled
        // area outside the project canvas reads as "letterbox" rather
        // than the platform default.
        self.scene.push_rect(daw_ui_renderer::RectCommand {
            rect: daw_ui_renderer::Rect::new(
                0.0,
                0.0,
                screen.width as f32,
                screen.height as f32,
            ),
            fill: Color::rgb(0.05, 0.05, 0.07),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });

        match self.frame_texture {
            Some(handle) if self.frame_size.0 > 0 && self.frame_size.1 > 0 => {
                let dst = aspect_fit_rect(
                    (screen.width as f32, screen.height as f32),
                    (self.frame_size.0 as f32, self.frame_size.1 as f32),
                );
                self.scene.push_textured_quad(TexturedQuad {
                    rect: daw_ui_renderer::Rect::new(dst.0, dst.1, dst.2, dst.3),
                    texture: handle,
                    alpha: 1.0,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: None,
                });
            }
            _ => {
                // No frame available — show the P4 placeholder text so
                // the user knows the window is alive but waiting on a
                // video clip / playhead.
                let text = "Video Preview";
                let approx_w = text.len() as f32 * 9.0;
                self.scene.push_text(GlyphArea {
                    text: text.into(),
                    left: (screen.width as f32 - approx_w) * 0.5,
                    top: (screen.height as f32 - 16.0) * 0.5,
                    font_size: 16.0,
                    line_height: 20.0,
                    color: Color::rgb(0.65, 0.7, 0.8),
                    clip_rect: None,
                });
            }
        }

        if let Err(e) = self.renderer.render(&self.scene) {
            tracing::error!(error = ?e, "preview render error");
        }
    }
}

/// Letterbox `src` into `dst`, centering with black bars on whichever
/// axis has slack. Returns `(x, y, w, h)` in destination coordinates.
fn aspect_fit_rect(dst: (f32, f32), src: (f32, f32)) -> (f32, f32, f32, f32) {
    let (dw, dh) = dst;
    let (sw, sh) = src;
    if sw <= 0.0 || sh <= 0.0 || dw <= 0.0 || dh <= 0.0 {
        return (0.0, 0.0, dw.max(0.0), dh.max(0.0));
    }
    let dst_aspect = dw / dh;
    let src_aspect = sw / sh;
    if src_aspect >= dst_aspect {
        // Source is wider — pillar-fit (top/bottom black bars).
        let h = dw / src_aspect;
        (0.0, (dh - h) * 0.5, dw, h)
    } else {
        // Source is taller — letterbox (left/right black bars).
        let w = dh * src_aspect;
        ((dw - w) * 0.5, 0.0, w, dh)
    }
}

/// Cap a project resolution so the preview window comfortably fits on
/// a typical laptop screen at boot. The user can resize the window
/// after creation; we just want a reasonable default.
///
/// Heuristic: scale (preserving aspect) so the longest dimension is
/// at most 960 logical pixels. 4K project → 960x540, 1080p → 960x540,
/// 720p → 960x540 (= identity), VGA → 640x480 (no scale-up either).
fn scale_to_fit_on_screen(size: (u32, u32)) -> (u32, u32) {
    let (w, h) = size;
    let (w, h) = (w.max(1), h.max(1));
    let max_dim = 960u32;
    let long = w.max(h);
    if long <= max_dim {
        return (w, h);
    }
    let scale = max_dim as f64 / long as f64;
    (
        ((w as f64) * scale).round().max(1.0) as u32,
        ((h as f64) * scale).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_to_fit_caps_long_dimension() {
        assert_eq!(scale_to_fit_on_screen((3840, 2160)), (960, 540));
        assert_eq!(scale_to_fit_on_screen((1920, 1080)), (960, 540));
        // 720p already under cap → identity.
        assert_eq!(scale_to_fit_on_screen((1280, 720)), (960, 540));
        // Below cap stays unchanged.
        assert_eq!(scale_to_fit_on_screen((640, 480)), (640, 480));
        // Pathological zeros clamp to >=1.
        let (w, h) = scale_to_fit_on_screen((0, 0));
        assert!(w >= 1 && h >= 1);
    }
}
