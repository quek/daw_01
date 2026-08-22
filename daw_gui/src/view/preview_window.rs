// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
//!   `WinitWindow` wrapper, and a `Scene`
//! - User clicks the window's close button → the runner notices the
//!   `WindowEvent::CloseRequested` and flips the field back to false,
//!   which destroys the state on the next frame

use std::sync::Arc;

use daw_ui_platform::WindowBackend;
use daw_ui_renderer::{
    Color, GlyphArea, HAlign, RenderError, Renderer, Scene, TextureHandle, TexturedQuad, VAlign,
};
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::{WindowAttributes, WindowId};

use daw_ui_platform::WinitWindow;

/// Per-window state owned by the runner while the preview is visible.
/// Dropped (= destroys the OS window) when `AppData.preview_window_visible`
/// transitions back to `false`.
pub struct PreviewWindowState {
    pub window: Arc<WinitWindow>,
    pub renderer: Renderer<WinitWindow>,
    pub scene: Scene,
    /// Per-`VideoSourceId` GPU texture the worker's decoded BGRA frame is
    /// uploaded into. The main thread creates the `TextureHandle` on the first
    /// frame (or a dimension change) and re-uses it for every subsequent frame
    /// of the same source. Keyed by `(VideoSourceId, slot_idx)`; with the libav
    /// BGRA sink `slot_idx` is always 0 (the per-slot lookahead ring was for the
    /// removed HW zero-copy path). `(handle, width, height)`; widths/heights are
    /// source-native (capped by the preview downscale), the composite pass
    /// aspect-fits each layer independently. `clear_all` releases everything
    /// when the preview window destructs.
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
    /// トラック合成画リスト。runner が毎 frame
    /// 構築し、bottom→top (track z 順) に並ぶ。各 `TrackComposite` は 1 トラックの
    /// 動画 / PiP 画像 / テキストを `items` に持ち、`render` が:
    /// - 効果も配置 transform も無ければ items を直接描く (plain track の fast-path)、
    /// - さもなくば `composite_scene_to_texture` で 1 枚へ合成 → track 効果チェーンを
    ///   `apply_chain` → 配置 transform (identity = canvas 全体 / group affine) で 1 quad push。
    ///
    /// 空 = placeholder text を表示。旧 `composite_layers`/`group_layers`/`text_layers` を統合。
    pub track_composites: Vec<crate::group_compose::TrackComposite>,
    /// マスター映像チェーン（`Song.master_fx_chain` の映像 device を
    /// 解決した実効効果）。空でなければ `render` が全トラック合成画を project
    /// 解像度の master canvas 1 枚に集約 → これをチェーン順 `apply_chain` → screen へ配置。
    /// runner が毎 frame `set_master_fx` で更新。
    pub master_fx: Vec<crate::video_fx::ResolvedEffect>,
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
    /// 選択中 clip が active visual group の子のとき、その親 group の解決済み
    /// transform（`docs/plan_tachie_group_transform.md` option A）。`Some` なら
    /// 選択オーバーレイを `CanvasMap::group` で group 空間へ写像して描く（=
    /// 親 group の移動 / 回転 / スケールにハンドルが追従）。group の子でない
    /// 通常 image / text overlay は `None`（= `CanvasMap::project` 恒等写像）。
    pub selection_group_transform: Option<common::model::GroupTransform>,
    /// `Song.video_resolution` の最新値 (width, height)。 PiP rect の
    /// normalized 0..=1 座標を「window 全体」 ではなく「project
    /// resolution が letterbox 配置された区域」 内で展開するために使う。
    /// runner が毎 frame で `set_project_resolution` を呼んで更新。
    /// preview window がリサイズされても画像 PiP の aspect ratio は
    /// project resolution に固定される (= 動画と同じ aspect-fit 動作)。
    pub project_resolution: (u32, u32),
    /// docs/plan_video_fx.md: トラック映像効果の GPU 実行基盤。
    /// `render` が composite layer / group layer の texture へ
    /// チェーン順適用する (pipeline cache + ping-pong pool を frame 跨ぎ保持)。
    pub fx_engine: crate::video_fx::VideoFxEngine,
}

// (r.md #8 F) 旧 `CompositeLayer` struct はここで定義のみで crate 内で一度も構築
// されないデッドコードだった (実際の preview 合成は `group_compose.rs` が担い、
// rotation_radians + rotation_pivot を `TexturedQuad` に正しく渡して回転も動作する)。
// 「wgpu pipeline は rotation 未対応」 のコメントは誤りだったので struct ごと削除。

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
        let dwin = Arc::new(WinitWindow::new(window));
        let renderer = Renderer::new(dwin.clone())
            .map_err(|e| format!("preview Renderer::new: {e}"))?;
        Ok(Self {
            window: dwin,
            renderer,
            scene: Scene::new(),
            frame_textures: std::collections::HashMap::new(),
            image_textures: std::collections::HashMap::new(),
            track_composites: Vec::new(),
            master_fx: Vec::new(),
            selection_overlay: None,
            selection_rotation_radians: 0.0,
            selection_group_transform: None,
            // 初期値は scale_to_fit_on_screen に渡された initial_size。
            // runner が `set_project_resolution` で `Song.video_resolution`
            // に同期させる前に preview window が描画されても、 1920x1080
            // 既定値があれば最初の 1 frame だけ少しズレるだけで以降は
            // 正しい aspect になる。
            project_resolution: initial_size,
            fx_engine: crate::video_fx::VideoFxEngine::new(),
        })
    }

    /// `Song.video_resolution` を毎 frame 同期。 preview composite が
    /// 画像 PiP の normalized 座標をこの解像度比で letterbox 配置するため、
    /// runner が `set_composite_layers` の隣で呼ぶ。
    pub fn set_project_resolution(&mut self, resolution: (u32, u32)) {
        self.project_resolution = resolution;
    }

    /// Update the PiP selection overlay (= 縁取り + corner / center
    /// handle 描画用)。 `None` で消す。 normalized 0..=1 座標。
    /// `rotation_radians` は縁取りと rotate handle を回転表示するため
    /// (= drag 中に視覚 feedback)。 `0.0` で axis-aligned 表示。
    /// `group_transform` は選択中 clip が active visual group の子のときその
    /// 親 group の解決済み transform（= 子枠を group 空間へ写像）。group の子で
    /// なければ `None`（= 恒等写像で従来どおり）。overlay と同じパスで毎 frame
    /// セットして SSoT を保つ。
    pub fn set_selection_overlay(
        &mut self,
        overlay: Option<(f32, f32, f32, f32)>,
        rotation_radians: f32,
        group_transform: Option<common::model::GroupTransform>,
    ) {
        self.selection_overlay = overlay;
        self.selection_rotation_radians = rotation_radians;
        self.selection_group_transform = group_transform;
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

    /// Upload a freshly-decoded BGRA frame into the `(source_id, slot_idx)`
    /// cache entry. Reuses the existing `TextureHandle` when the dimensions
    /// match (= hot path during playback); on a dimension change (= rare, would
    /// mean the project's video sources were re-imported) destroys and
    /// re-creates the texture for that slot.
    ///
    /// libav decodes every source to a system-memory BGRA8 frame — there is one
    /// decode path now; the old zero-copy D3D11 `Shared` import went away with
    /// Media Foundation (`docs/plan_video_decode_unify.md`). The BGRA sink is
    /// 1-frame-latest, so the worker only ever fills slot 0.
    pub fn upload_frame(
        &mut self,
        source_id: common::model::VideoSourceId,
        slot_idx: u8,
        frame: &crate::video_playback::DecodedFrame,
    ) -> Option<TextureHandle> {
        let key = (source_id, slot_idx);
        let recreate = match self.frame_textures.get(&key) {
            Some((_, w, h)) => *w != frame.width || *h != frame.height,
            None => true,
        };
        if recreate {
            if let Some((old, _, _)) = self.frame_textures.remove(&key) {
                self.renderer.destroy_texture(old);
            }
            let h = self.renderer.create_texture_bgra(frame.width, frame.height);
            self.frame_textures.insert(key, (h, frame.width, frame.height));
        }
        let handle = self
            .frame_textures
            .get(&key)
            .map(|(h, _, _)| *h)
            .expect("just inserted");
        self.renderer.upload_texture_bgra(handle, &frame.bgra);
        Some(handle)
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
        self.track_composites.clear();
    }

    /// トラック合成画リストを毎 frame 更新。runner が
    /// `render` の前に bottom→top (track z 順) で渡す。旧
    /// `set_composite_layers`/`set_group_layers`/`set_text_layers` を統合。
    pub fn set_track_composites(
        &mut self,
        composites: Vec<crate::group_compose::TrackComposite>,
    ) {
        self.track_composites = composites;
    }

    /// マスター映像チェーンの解決済み効果を毎 frame 更新（runner）。
    /// 空でなければ `render` が master canvas 1 枚に集約してから適用する。
    pub fn set_master_fx(&mut self, fx: Vec<crate::video_fx::ResolvedEffect>) {
        self.master_fx = fx;
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

    /// GPU device が失われた preview の資産を丸ごと作り直す (r.md #42)。
    ///
    /// **OS ウィンドウは破棄しない**。`state.preview = None` で窓ごと作り直すと位置と
    /// サイズがリセットされ一瞬消えて再表示されるので、中身 (wgpu device / 各テクスチャ /
    /// 効果エンジン) だけを差し替える。
    ///
    /// - `frame_textures` / `image_textures`: 旧 device の handle なので `destroy` せずに
    ///   捨てる (死んだ store に触るだけで無意味。実体は旧 device と一緒に解放される)。
    ///   動画フレームは次の decode で、画像は `pending_image_uploads` の再投入で戻る。
    /// - `fx_engine`: pool / pipeline / feedback history がすべて旧 device のものなので
    ///   個別に片付けようとせず [`VideoFxEngine::new`] で丸ごと差し替える。
    ///
    /// # Errors
    /// wgpu 再初期化に失敗 (= まだ GPU が戻っていない)。caller は backoff して再試行する。
    pub fn recreate_gpu(&mut self) -> Result<(), String> {
        self.renderer
            .recreate()
            .map_err(|e| format!("preview Renderer::recreate: {e}"))?;
        self.frame_textures.clear();
        self.image_textures.clear();
        self.fx_engine = crate::video_fx::VideoFxEngine::new();
        self.track_composites.clear();
        self.master_fx.clear();
        Ok(())
    }

    /// Build the scene + render. docs/plan_video.md P7: walks
    /// `composite_layers` bottom→top and pushes one aspect-fit
    /// textured quad per layer on top of the dark backdrop. gui_01's
    /// call-order interleave gives standard "src over dst" blending
    /// so the topmost track wins at `alpha=1.0` and crossfades mix
    /// at intermediate alphas. Empty layer list falls back to the
    /// P4 placeholder text.
    ///
    /// この窓は自前の [`Renderer`] / [`Scene`] を持ち `Ui` を通らないので、色は
    /// `ui.palette()` ではなく引数の `theme` から取る (r.md #48)。使うのは
    /// `theme.daw.video_*` = **極性固定トークン**で、両テーマ同値。映像の外側
    /// (レターボックス) が暗いままなのは書き出し動画の黒背景と対だからで、
    /// `--smoke-test` の near-black 判定もこれを前提にしている。
    ///
    /// # Errors
    /// 描画できなかった理由 ([`RenderError`])。`DeviceLost` なら caller (runner) が
    /// [`Self::recreate_gpu`] を含む復旧シーケンスを駆動する。
    pub fn render(&mut self, theme: &crate::theme::Theme) -> Result<(), RenderError> {
        // GPU 消失中は合成も効果適用も全部無駄 (かつ死んだ device を触る) なので、
        // scene を組む前に抜ける。復旧は runner が駆動する。
        if !self.renderer.is_live() {
            return Err(RenderError::DeviceLost);
        }
        // 前 frame の効果 target を解放 (前 frame は末尾の render() で sample 済み)。
        // これで今 frame の apply_chain が同寸でも別 target を払い出し、レイヤー間衝突を防ぐ
        // (gui_01 CompositePool::end_cycle を render 冒頭で呼ぶのと同 idiom)。
        self.fx_engine.end_frame(&mut self.renderer);
        self.scene.clear();
        let daw = &theme.daw;
        let screen = self.renderer.size();
        // Dark backdrop spanning the entire window so any unfilled
        // area outside the project canvas reads as "letterbox" rather
        // than the platform default. ライトテーマでも暗いままなのが仕様
        // (`video_canvas_bg` は両テーマ同値の極性固定トークン)。
        self.scene.push_rect(daw_ui_renderer::RectCommand {
            rect: daw_ui_renderer::Rect::new(
                0.0,
                0.0,
                screen.width as f32,
                screen.height as f32,
            ),
            fill: daw.video_canvas_bg,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });

        // PiP rect / text rect の normalized 0..=1 は「project_resolution
        // が preview window 内で letterbox 配置された区域」 内の座標として
        // 扱う。 これで window resize しても画像 aspect ratio は project
        // 比 (= 動画 letterbox と同じ) に固定される。
        let project_box = aspect_fit_rect(
            (screen.width as f32, screen.height as f32),
            (
                self.project_resolution.0 as f32,
                self.project_resolution.1 as f32,
            ),
        );

        if self.track_composites.is_empty() {
            // No content — show the P4 placeholder text so the user knows the
            // window is alive but waiting on a clip / playhead.
            // 「1 文字 9px」 の概算幅で中央を出すと実 advance (16 * 0.527 = 8.44px)
            // とずれて左に寄る。 GlyphArea の box + align に任せて renderer 側の
            // 実測センタリングを使う (Ui を持たない窓なので measure_text は呼べない)。
            self.scene.push_text(GlyphArea {
                box_width: Some(screen.width as f32),
                box_height: Some(screen.height as f32),
                align_h: HAlign::Center,
                align_v: VAlign::Center,
                ..GlyphArea::new(
                    "Video Preview".into(),
                    0.0,
                    0.0,
                    16.0,
                    20.0,
                    daw.video_placeholder_text,
                )
            });
        } else {
            // トラック合成画を bottom→top に描く。borrow 分離のため
            // 一旦 take して iterate。runner が毎 frame 再設定するので take しても問題ない。
            let composites = std::mem::take(&mut self.track_composites);
            let master_fx = std::mem::take(&mut self.master_fx);
            if master_fx.is_empty() {
                // 通常: 各トラック合成画を screen project_box へ直接描く（fast-path 含む）。
                for tc in &composites {
                    self.draw_track_composite(tc, project_box, daw);
                }
            } else {
                // master 映像チェーン。全トラック合成画を project 解像度の
                // master canvas 1 枚に集約 → master fx をチェーン順適用 → screen project_box へ
                // 配置（export と同一 SSoT）。overlay は UI なので master fx の対象外、合成後に
                // screen 座標で別途描く。
                let (pw, ph) = self.project_resolution;
                let (pw, ph) = (pw.max(1), ph.max(1));
                let content_box = (0.0, 0.0, pw as f32, ph as f32);
                let mut content = Scene::new();
                for tc in &composites {
                    crate::group_compose::composite_and_place(
                        tc,
                        content_box,
                        self.project_resolution,
                        &mut self.renderer,
                        &mut self.fx_engine,
                        &mut content,
                    );
                }
                match self.renderer.composite_scene_to_texture(&content, pw, ph) {
                    Ok(handle) => {
                        let handle = self.fx_engine.apply_chain(
                            &mut self.renderer,
                            handle,
                            pw,
                            ph,
                            &master_fx,
                            crate::video_fx::MASTER_CHAIN_KEY,
                        );
                        self.scene.push_textured_quad(TexturedQuad {
                            rect: daw_ui_renderer::Rect::new(
                                project_box.0,
                                project_box.1,
                                project_box.2,
                                project_box.3,
                            ),
                            texture: handle,
                            alpha: 1.0,
                            uv_min: (0.0, 0.0),
                            uv_max: (1.0, 1.0),
                            clip_rect: None,
                            rotation_radians: 0.0,
                            rotation_pivot: None,
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "master 映像 composite 失敗"),
                }
                // 選択中トラックの Transform overlay を screen 座標で（master fx 後）。
                for tc in &composites {
                    if tc.selected && let Some(t) = tc.transform {
                        self.draw_group_overlay(&t, project_box, daw);
                    }
                }
            }
            self.track_composites = composites;
            self.master_fx = master_fx;
        }
        self.draw_selection_overlay(screen.width as f32, screen.height as f32, daw);

        self.renderer.render(&self.scene)
    }

    /// 1 トラック合成画を描く。効果も配置
    /// transform も無ければ items を直接 screen px へ描く (plain track の fast-path =
    /// 現状維持・クリスプ・無コスト)。さもなくば items を canvas へ 1 枚合成 →
    /// track 効果チェーンを `apply_chain` → 配置 transform (identity = canvas 全体 /
    /// group affine = approach X) で 1 quad push。
    fn draw_track_composite(
        &mut self,
        tc: &crate::group_compose::TrackComposite,
        project_box: (f32, f32, f32, f32),
        daw: &crate::theme::DawColors,
    ) {
        // 合成 + 配置は preview / export 共通の SSoT 経路（byte parity）。
        crate::group_compose::composite_and_place(
            tc,
            project_box,
            self.project_resolution,
            &mut self.renderer,
            &mut self.fx_engine,
            &mut self.scene,
        );
        // 選択中 group / Transform は bounding box + handle を描く（preview のみ）。
        if tc.selected && let Some(t) = tc.transform {
            self.draw_group_overlay(&t, project_box, daw);
        }
    }

    /// 選択中 group / Transform の bounding box + anchor 十字 + rotate/scale ハンドルを
    /// 描く。描画 quad と同一の [`group_quad_params`](crate::group_compose::group_quad_params)
    /// を使うので位置は完全一致（近似なし）。
    fn draw_group_overlay(
        &mut self,
        t: &common::model::GroupTransform,
        project_box: (f32, f32, f32, f32),
        daw: &crate::theme::DawColors,
    ) {
        let (rx, ry, rw, rh, rot, px, py, _alpha) =
            crate::group_compose::group_quad_params(t, project_box);
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        let pivx = rx + px;
        let pivy = ry + py;
        let (sin_r, cos_r) = rot.sin_cos();
        let rotate_pt = |sx: f32, sy: f32| -> [f32; 2] {
            let lx = sx - pivx;
            let ly = sy - pivy;
            [pivx + lx * cos_r - ly * sin_r, pivy + lx * sin_r + ly * cos_r]
        };
        let c0 = rotate_pt(rx, ry);
        let c1 = rotate_pt(rx + rw, ry);
        let c2 = rotate_pt(rx + rw, ry + rh);
        let c3 = rotate_pt(rx, ry + rh);
        // 親グループの範囲枠。暗い映像面の上に置く極性固定トークン (r.md #48)。
        let stroke = daw.video_group_outline;
        let edges = vec![
            daw_ui_renderer::LineSegment { a: c0, b: c1, color: stroke },
            daw_ui_renderer::LineSegment { a: c1, b: c2, color: stroke },
            daw_ui_renderer::LineSegment { a: c2, b: c3, color: stroke },
            daw_ui_renderer::LineSegment { a: c3, b: c0, color: stroke },
        ];
        self.scene.push_lines(daw_ui_renderer::LineBatch {
            segments: std::sync::Arc::from(edges),
            line_width_px: 2.0,
            clip_rect: None,
        });
        // anchor marker: pivot（= 回転・スケール中心）に小さな十字。
        const AH: f32 = 7.0;
        let cross = vec![
            daw_ui_renderer::LineSegment { a: [pivx - AH, pivy], b: [pivx + AH, pivy], color: stroke },
            daw_ui_renderer::LineSegment { a: [pivx, pivy - AH], b: [pivx, pivy + AH], color: stroke },
        ];
        self.scene.push_lines(daw_ui_renderer::LineBatch {
            segments: std::sync::Arc::from(cross),
            line_width_px: 2.0,
            clip_rect: None,
        });
        // rotate handle: 上辺中点から外側 24px（runner の group_hit_test の Rotate 判定位置と一致）。
        let top_mid = rotate_pt(rx + rw * 0.5, ry);
        let rot_knob = rotate_pt(rx + rw * 0.5, ry - 24.0);
        self.scene.push_lines(daw_ui_renderer::LineBatch {
            segments: std::sync::Arc::from(vec![daw_ui_renderer::LineSegment {
                a: top_mid,
                b: rot_knob,
                color: stroke,
            }]),
            line_width_px: 2.0,
            clip_rect: None,
        });
        // handle ノブ（rotate + 4 corner scale）を小四角で描画（box 角 = Resize 判定位置）。
        const KNOB: f32 = 9.0;
        for [hx, hy] in [rot_knob, c0, c1, c2, c3] {
            self.scene.push_rect(daw_ui_renderer::RectCommand {
                rect: daw_ui_renderer::Rect::new(hx - KNOB * 0.5, hy - KNOB * 0.5, KNOB, KNOB),
                fill: stroke,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
    }

    /// `selection_overlay` を screen px に変換し、 縁取り + 4 corner +
    /// center + rotate handle を scene に push する。 `selection
    /// _overlay` が `None` ならただ早期 return。 縁取りは rect 中心
    /// 旋回で `selection_rotation_radians` を反映 (= 画像と一緒に回る)。
    /// 4 corner handle / center handle / rotate handle 位置も同様に
    /// 回転後座標で描画 (`docs/plan_image_automation.md` rotation)。
    fn draw_selection_overlay(&mut self, sw: f32, sh: f32, daw: &crate::theme::DawColors) {
        let Some((nx, ny, nw, nh)) = self.selection_overlay else {
            return;
        };
        // PiP rect は project_resolution の letterbox 内座標系 (画像
        // 描画と同 idiom)。 window resize しても画像と縁取りが一致する。
        let project_box = aspect_fit_rect(
            (sw, sh),
            (self.project_resolution.0 as f32, self.project_resolution.1 as f32),
        );
        // 選択中 clip が active visual group の子なら group 空間へ写像する
        // （= 縁取り / ハンドルが親 group の移動・回転・スケールに追従）。
        // group の子でなければ恒等写像（canvas == project_box）で従来どおり。
        let map = match self.selection_group_transform {
            Some(t) => crate::group_compose::CanvasMap::group(&t, project_box),
            None => crate::group_compose::CanvasMap::project(project_box),
        };
        // rect 中心 = 子 PiP 中心を canvas→screen 写像。half 寸法は canvas 軸上の
        // 長さ（group の非一様 scale を含む）。総回転 = group 軸回転 + 子自身の
        // 回転。child rotation = 0 + group なら厳密に回転長方形（shear なし）。
        let (cx, cy) = map.to_screen(nx + nw * 0.5, ny + nh * 0.5);
        let rw = nw * map.size.0;
        let rh = nh * map.size.1;
        let rot = map.rotation + self.selection_rotation_radians;
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
        // 選択枠。暗い映像面の上の極性固定トークン (r.md #48)。
        let stroke_color = daw.video_selection_stroke;
        const STROKE_W: f32 = 2.0;
        let edge_pts: Vec<daw_ui_renderer::LineSegment> = vec![
            daw_ui_renderer::LineSegment {
                a: [nw_p.0, nw_p.1],
                b: [ne_p.0, ne_p.1],
                color: stroke_color,
            },
            daw_ui_renderer::LineSegment {
                a: [ne_p.0, ne_p.1],
                b: [se_p.0, se_p.1],
                color: stroke_color,
            },
            daw_ui_renderer::LineSegment {
                a: [se_p.0, se_p.1],
                b: [sw_p.0, sw_p.1],
                color: stroke_color,
            },
            daw_ui_renderer::LineSegment {
                a: [sw_p.0, sw_p.1],
                b: [nw_p.0, nw_p.1],
                color: stroke_color,
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
                color: stroke_color,
            }]),
            line_width_px: STROKE_W,
            clip_rect: None,
        };
        self.scene.push_lines(rot_line);
        // Corner / center / rotate handle (= 6 個)。 handle 自体は
        // axis-aligned rect で描画 (= 回転後の中心位置に小 square)。
        const HANDLE: f32 = 10.0;
        // 選択ハンドルと、その縁取り (映像の上で必ず立つ純黒)。両テーマ同値の極性固定トークン。
        let handle_color = daw.video_handle;
        let handle_border = daw.video_handle_border;
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
                fill: handle_color,
                border: handle_border,
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
/// at most 640 logical pixels. 4K project → 640x360, 1080p → 640x360,
/// 720p → 640x360, VGA (640x480) → 640x480 (長辺がちょうど上限なので等倍)。
/// r.md #25: 既定サイズを小さめにして、 preview を常時表示しても main
/// window の作業を邪魔しないようにする (user は自由にリサイズできる)。
fn scale_to_fit_on_screen(size: (u32, u32)) -> (u32, u32) {
    let (w, h) = size;
    let (w, h) = (w.max(1), h.max(1));
    let max_dim = 640u32;
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
        assert_eq!(scale_to_fit_on_screen((3840, 2160)), (640, 360));
        assert_eq!(scale_to_fit_on_screen((1920, 1080)), (640, 360));
        // 720p も長辺 1280 > 640 上限なので縮小される。
        assert_eq!(scale_to_fit_on_screen((1280, 720)), (640, 360));
        // 長辺がちょうど上限 (VGA 640x480) は等倍のまま。
        assert_eq!(scale_to_fit_on_screen((640, 480)), (640, 480));
        // Pathological zeros clamp to >=1.
        let (w, h) = scale_to_fit_on_screen((0, 0));
        assert!(w >= 1 && h >= 1);
    }
}
