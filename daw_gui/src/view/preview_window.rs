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
    /// docs/plan_video_perf.md P4: per-`(VideoSourceId, slot_idx)`
    /// GPU textures backing the lookahead ring. The worker
    /// round-robins decoded frames through `PREVIEW_RING_SIZE`
    /// independent `SharedPool` slots; the main thread imports each
    /// slot's stable NT handle (zero-copy path) or uploads BGRA bytes
    /// (CPU fallback path) into its own `TextureHandle` here exactly
    /// once per slot, then re-uses that handle for every subsequent
    /// frame the worker writes into the same slot. `(handle, width,
    /// height)`; widths/heights are source-native, the composite pass
    /// aspect-fits each layer into the preview window independently.
    /// `clear_all` releases everything when the preview window
    /// destructs.
    pub frame_textures: std::collections::HashMap<
        (common::model::VideoSourceId, u8),
        (TextureHandle, u32, u32),
    >,
    /// docs/plan_image_overlay.md §P3: per-`ImageSourceId` GPU textures
    /// backing the PiP overlay path. Static — the import path uploads
    /// each image once via `upload_image_bgra` and the handle stays
    /// valid for the lifetime of the preview window. `(handle, width,
    /// height)`.
    pub image_textures: std::collections::HashMap<
        common::model::ImageSourceId,
        (TextureHandle, u32, u32),
    >,
    /// docs/plan_video.md P7: composite layer list the runner pushes
    /// each frame, ordered bottom→top (= lowest video track first,
    /// crossfade-partner clip on top of fading-out clip). Each entry
    /// references a `frame_textures` handle. Cleared at the start of
    /// every frame and refilled by the runner before
    /// `render_placeholder`. Empty = the placeholder text appears.
    pub composite_layers: Vec<CompositeLayer>,
    /// `docs/plan_image_automation.md` §5 / `plan_image_overlay.md` §4
    /// P5: 選択中 image event の PiP rect (normalized 0..=1)。 `Some`
    /// なら render pass が縁取り + 4 corner handle + center handle を
    /// composite の上に push する。 `None` ならオーバーレイなし。
    /// runner が毎 frame で `set_selection_overlay` を呼んで更新。
    pub selection_overlay: Option<(f32, f32, f32, f32)>,
    /// 選択中 image event の rotation_radians (= 縁取り + rotate handle
    /// 描画時に rect を回転させて表示する)。 lane override 値が乗った
    /// 結果が入る (= runner 経由)。
    pub selection_rotation_radians: f32,
}

/// One textured layer in the preview composite. The runner builds a
/// `Vec<CompositeLayer>` ordered bottom→top each frame; the render
/// pass calls `push_textured_quad` in that order so gui_01's
/// call-order interleave lays the top track / crossfade-target on
/// top.
///
/// `pip_rect` (docs/plan_image_overlay.md §P3) selects between two
/// placement modes:
/// - `None` — aspect-fit letterbox over the entire preview window
///   (= the video clip default behaviour, unchanged from pre-P3).
/// - `Some((x, y, w, h))` — normalized 0-1 PiP rect; the renderer
///   maps `(x, y, w, h)` to screen px and draws the layer at exactly
///   that sub-rect of the preview surface, regardless of aspect.
///   Used by `ClipContent::Image` for ロゴ / ジャケット overlays.
#[derive(Debug, Clone, Copy)]
pub struct CompositeLayer {
    pub texture: TextureHandle,
    pub width: u32,
    pub height: u32,
    pub alpha: f32,
    pub pip_rect: Option<(f32, f32, f32, f32)>,
    /// v15 (`docs/plan_image_automation.md` rotation): rect 中心を旋回
    /// 中心とする 2D 回転 (radians、 clockwise positive)。 PiP layer (=
    /// `pip_rect = Some`) でのみ意味を持ち、 video aspect-fit layer は
    /// 常に `0.0`。 `gui_01 #047` (`TexturedQuad.rotation_radians`)
    /// landing 後に `push_textured_quad` に渡す。 現状の wgpu pipeline
    /// は rotation 未対応のため値は保持のみ、 描画は axis-aligned で
    /// 走る。
    pub rotation_radians: f32,
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
        owner_hwnd: Option<isize>,
    ) -> Result<Self, String> {
        let (w, h) = scale_to_fit_on_screen(initial_size);
        let attrs = WindowAttributes::default()
            .with_title("daw_01 — Video Preview")
            .with_inner_size(LogicalSize::new(w, h));
        // Windows: owner window を設定すると preview は main より常に前面、
        // main 最小化で preview も最小化、 タスクバーには出ない (= MV
        // プレビューを別ウィンドウで常時見えるようにする UX)。 winit の
        // `with_owner_window` は `isize` (= HWND alias) を直接受ける。
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            match owner_hwnd {
                Some(h) if h != 0 => attrs.with_owner_window(h),
                _ => attrs,
            }
        };
        #[cfg(not(windows))]
        let _ = owner_hwnd;
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
            frame_textures: std::collections::HashMap::new(),
            image_textures: std::collections::HashMap::new(),
            composite_layers: Vec::new(),
            selection_overlay: None,
            selection_rotation_radians: 0.0,
        })
    }

    /// Update the PiP selection overlay (= 縁取り + corner / center
    /// handle 描画用)。 `None` で消す。 normalized 0..=1 座標。
    /// `rotation_radians` は縁取りと rotate handle を回転表示するため
    /// (= drag 中に視覚 feedback)。 `0.0` で axis-aligned 表示。
    pub fn set_selection_overlay(
        &mut self,
        overlay: Option<(f32, f32, f32, f32)>,
        rotation_radians: f32,
    ) {
        self.selection_overlay = overlay;
        self.selection_rotation_radians = rotation_radians;
    }

    /// docs/plan_image_overlay.md §P3: upload a freshly-decoded image
    /// into its dedicated `(ImageSourceId)` GPU texture. Idempotent —
    /// re-uploading the same id replaces the existing texture (=
    /// reimport-after-edit case). Returns the cached handle so the
    /// caller can populate `AppData::image_texture_cache`.
    pub fn upload_image_bgra(
        &mut self,
        source_id: common::model::ImageSourceId,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> TextureHandle {
        if let Some((old, _, _)) = self.image_textures.remove(&source_id) {
            self.renderer.destroy_texture(old);
        }
        let handle = self.renderer.create_texture_bgra(width, height);
        self.renderer.upload_texture_bgra(handle, bgra);
        self.image_textures.insert(source_id, (handle, width, height));
        handle
    }

    /// Upload (or `Shared` import) a freshly-decoded frame into the
    /// `(source_id, slot_idx)` cache entry. Reuses the existing
    /// `TextureHandle` when the dimensions match (= hot path during
    /// playback); on a dimension change (= rare, would mean the
    /// project's video sources were re-imported) destroys and re-creates
    /// the texture for that specific slot.
    ///
    /// docs/plan_video_perf.md P4: per-slot caching. Each
    /// `VideoSourceId` owns `PREVIEW_RING_SIZE` distinct slots; their
    /// shared NT handles (HW path) are stable for the slot's lifetime
    /// so import happens exactly once per `(source_id, slot_idx)`
    /// pair. Subsequent frames the worker writes into the same slot
    /// reuse the cached `TextureHandle`.
    ///
    /// Two paths, dispatched on the `DecodedFrame` variant:
    ///
    /// - `Shared` (zero-copy, P3): the variant's own `slot_idx` is
    ///   used as the cache key; we ignore the caller's `slot_idx`
    ///   argument and read the one embedded in the frame to avoid
    ///   mismatch.
    /// - `Bgra` (CPU fallback, P2): the variant has no slot field
    ///   (the ring is HW-path only); we use the caller's `slot_idx`
    ///   verbatim so the worker can still write all N ring slots
    ///   into independent CPU textures, though the gains are limited
    ///   compared to the GPU path.
    pub fn upload_frame(
        &mut self,
        source_id: common::model::VideoSourceId,
        slot_idx: u8,
        frame: &crate::video_playback::DecodedFrame,
    ) -> Option<TextureHandle> {
        use crate::video_playback::DecodedFrame;
        match frame {
            DecodedFrame::Shared {
                width,
                height,
                handle,
                slot_idx: frame_slot,
            } => {
                let key = (source_id, *frame_slot);
                if let Some((_, w, h)) = self.frame_textures.get(&key)
                    && *w == *width
                    && *h == *height
                {
                    // Already imported — the worker writes new content
                    // into the same underlying D3D11 resource on every
                    // frame, so nothing for us to do here.
                    return self.frame_textures.get(&key).map(|(h, _, _)| *h);
                }
                // First frame for this (source, slot) (or dimensions
                // changed): import the DXGI shared NT handle into
                // wgpu's texture pool exactly once.
                if let Some((old, _, _)) = self.frame_textures.remove(&key) {
                    self.renderer.destroy_texture(old);
                }
                match self.renderer.create_texture_from_d3d11_shared_handle(
                    handle.0,
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    *width,
                    *height,
                ) {
                    Ok(tex) => {
                        self.frame_textures.insert(key, (tex, *width, *height));
                        Some(tex)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            video_source_id = source_id,
                            slot_idx = *frame_slot,
                            "create_texture_from_d3d11_shared_handle failed"
                        );
                        None
                    }
                }
            }
            DecodedFrame::Bgra { width, height, bgra } => {
                let key = (source_id, slot_idx);
                let recreate = match self.frame_textures.get(&key) {
                    Some((_, w, h)) => *w != *width || *h != *height,
                    None => true,
                };
                if recreate {
                    if let Some((old, _, _)) = self.frame_textures.remove(&key) {
                        self.renderer.destroy_texture(old);
                    }
                    let h = self.renderer.create_texture_bgra(*width, *height);
                    self.frame_textures.insert(key, (h, *width, *height));
                }
                let handle = self
                    .frame_textures
                    .get(&key)
                    .map(|(h, _, _)| *h)
                    .expect("just inserted");
                self.renderer.upload_texture_bgra(handle, bgra);
                Some(handle)
            }
        }
    }

    /// Drop every cached frame texture and clear the composite list
    /// (= called when the preview window is about to be destroyed so
    /// the GPU side releases everything cleanly).
    pub fn clear_all(&mut self) {
        for (_, (h, _, _)) in self.frame_textures.drain() {
            self.renderer.destroy_texture(h);
        }
        for (_, (h, _, _)) in self.image_textures.drain() {
            self.renderer.destroy_texture(h);
        }
        self.composite_layers.clear();
    }

    /// Refresh the per-frame composite layer list. Called by the
    /// runner each frame BEFORE `render_placeholder` with the
    /// bottom→top stack of (texture, dimensions, alpha) tuples.
    /// Replaces any previous frame's layers so the preview never
    /// shows stale content.
    pub fn set_composite_layers(&mut self, layers: Vec<CompositeLayer>) {
        self.composite_layers = layers;
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

    /// Build the scene + render. docs/plan_video.md P7: walks
    /// `composite_layers` bottom→top and pushes one aspect-fit
    /// textured quad per layer on top of the dark backdrop. gui_01's
    /// call-order interleave gives standard "src over dst" blending
    /// so the topmost track wins at `alpha=1.0` and crossfades mix
    /// at intermediate alphas. Empty layer list falls back to the
    /// P4 placeholder text.
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

        if self.composite_layers.is_empty() {
            // No frame available — show the P4 placeholder text so the
            // user knows the window is alive but waiting on a video
            // clip / playhead.
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
            self.draw_selection_overlay(screen.width as f32, screen.height as f32);
        } else {
            for layer in &self.composite_layers {
                if layer.width == 0 || layer.height == 0 || layer.alpha <= 0.0 {
                    continue;
                }
                // docs/plan_image_overlay.md §P3: PiP rect handling.
                // Video clips (`pip_rect = None`) letterbox; image
                // overlays (`pip_rect = Some(x,y,w,h)` in normalized
                // 0-1) map to a sub-rect of the preview surface.
                let dst = match layer.pip_rect {
                    None => aspect_fit_rect(
                        (screen.width as f32, screen.height as f32),
                        (layer.width as f32, layer.height as f32),
                    ),
                    Some((nx, ny, nw, nh)) => (
                        nx * screen.width as f32,
                        ny * screen.height as f32,
                        nw * screen.width as f32,
                        nh * screen.height as f32,
                    ),
                };
                self.scene.push_textured_quad(TexturedQuad {
                    rect: daw_ui_renderer::Rect::new(dst.0, dst.1, dst.2, dst.3),
                    texture: layer.texture,
                    alpha: layer.alpha,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: None,
                    rotation_radians: layer.rotation_radians,
                });
            }
            self.draw_selection_overlay(screen.width as f32, screen.height as f32);
        }

        if let Err(e) = self.renderer.render(&self.scene) {
            tracing::error!(error = ?e, "preview render error");
        }
    }

    /// `selection_overlay` を screen px に変換し、 縁取り + 4 corner +
    /// center + rotate handle を scene に push する。 `selection
    /// _overlay` が `None` ならただ早期 return。 縁取りは rect 中心
    /// 旋回で `selection_rotation_radians` を反映 (= 画像と一緒に回る)。
    /// 4 corner handle / center handle / rotate handle 位置も同様に
    /// 回転後座標で描画 (`docs/plan_image_automation.md` rotation)。
    fn draw_selection_overlay(&mut self, sw: f32, sh: f32) {
        let Some((nx, ny, nw, nh)) = self.selection_overlay else {
            return;
        };
        // PiP rect (= 画像の表示領域) を screen px に変換。
        let rx = nx * sw;
        let ry = ny * sh;
        let rw = nw * sw;
        let rh = nh * sh;
        let cx = rx + rw * 0.5;
        let cy = ry + rh * 0.5;
        let rot = self.selection_rotation_radians;
        let (sin_r, cos_r) = rot.sin_cos();
        // (cx 基準の local x, y) → screen の (px, py)。
        let rotate = |lx: f32, ly: f32| -> (f32, f32) {
            (cx + lx * cos_r - ly * sin_r, cy + lx * sin_r + ly * cos_r)
        };
        let half_w = rw * 0.5;
        let half_h = rh * 0.5;
        // 4 corner (回転前 local → 回転後 screen)。
        let nw_p = rotate(-half_w, -half_h);
        let ne_p = rotate(half_w, -half_h);
        let se_p = rotate(half_w, half_h);
        let sw_p = rotate(-half_w, half_h);
        // 縁取り 4 edge を line で描画。 push_lines は 1 batch で
        // 複数 segment OK。 LineSegment の field は `a: [f32; 2]` /
        // `b: [f32; 2]` / `color`。
        const STROKE_COLOR: Color = Color { r: 1.0, g: 0.95, b: 0.45, a: 0.85 };
        const STROKE_W: f32 = 2.0;
        let edge_pts: Vec<daw_ui_renderer::LineSegment> = vec![
            daw_ui_renderer::LineSegment {
                a: [nw_p.0, nw_p.1],
                b: [ne_p.0, ne_p.1],
                color: STROKE_COLOR,
            },
            daw_ui_renderer::LineSegment {
                a: [ne_p.0, ne_p.1],
                b: [se_p.0, se_p.1],
                color: STROKE_COLOR,
            },
            daw_ui_renderer::LineSegment {
                a: [se_p.0, se_p.1],
                b: [sw_p.0, sw_p.1],
                color: STROKE_COLOR,
            },
            daw_ui_renderer::LineSegment {
                a: [sw_p.0, sw_p.1],
                b: [nw_p.0, nw_p.1],
                color: STROKE_COLOR,
            },
        ];
        self.scene.push_lines(daw_ui_renderer::LineBatch {
            segments: std::sync::Arc::from(edge_pts),
            line_width_px: STROKE_W,
            clip_rect: None,
        });
        // rotate handle: 上辺中点から外側 24 px (= 回転前 (0, -half_h
        // - 24))。 line で center と繋ぐ。
        const ROTATE_OFFSET: f32 = 24.0;
        let rotate_p = rotate(0.0, -half_h - ROTATE_OFFSET);
        let top_mid = rotate(0.0, -half_h);
        let rot_line = daw_ui_renderer::LineBatch {
            segments: std::sync::Arc::from(vec![daw_ui_renderer::LineSegment {
                a: [top_mid.0, top_mid.1],
                b: [rotate_p.0, rotate_p.1],
                color: STROKE_COLOR,
            }]),
            line_width_px: STROKE_W,
            clip_rect: None,
        };
        self.scene.push_lines(rot_line);
        // Corner / center / rotate handle (= 6 個)。 handle 自体は
        // axis-aligned rect で描画 (= 回転後の中心位置に小 square)。
        const HANDLE: f32 = 10.0;
        const HANDLE_COLOR: Color = Color { r: 1.0, g: 0.95, b: 0.45, a: 1.0 };
        const HANDLE_BORDER: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.85 };
        let handle_centers = [
            nw_p,
            ne_p,
            sw_p,
            se_p,
            (cx, cy),
            rotate_p,
        ];
        for (hx, hy) in handle_centers {
            self.scene.push_rect(daw_ui_renderer::RectCommand {
                rect: daw_ui_renderer::Rect::new(
                    hx - HANDLE * 0.5,
                    hy - HANDLE * 0.5,
                    HANDLE,
                    HANDLE,
                ),
                fill: HANDLE_COLOR,
                border: HANDLE_BORDER,
                border_width: 1.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
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
