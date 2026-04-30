//! glyphon 統合 — テキスト描画パイプライン。
//!
//! M1 では毎フレーム `Buffer` を作り直すシンプル構成。
//! 性能最適化 (Buffer の hash キャッシュ) は M3 の scenegraph 化と一緒にやる。

use daw_ui_platform::PhysicalSize;
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::MultisampleState;

use crate::scene::GlyphArea;

pub struct GlyphPipeline {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// prepare で構築した Buffer 群。render 後 clear。
    buffers: Vec<Buffer>,
}

impl GlyphPipeline {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, target_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            buffers: Vec::new(),
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyph_areas: &[GlyphArea],
        screen: PhysicalSize,
    ) {
        self.buffers.clear();
        self.viewport.update(
            queue,
            Resolution { width: screen.width, height: screen.height },
        );

        if glyph_areas.is_empty() {
            // 空でも prepare 呼んでおかないと前フレームの描画が残る場合がある
            let _ = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                std::iter::empty::<TextArea<'_>>(),
                &mut self.swash_cache,
            );
            return;
        }

        // 1) 各 GlyphArea 毎に Buffer を構築 (Vec に保持して借用元にする)
        for area in glyph_areas {
            let metrics = Metrics::new(area.font_size, area.line_height);
            let mut buffer = Buffer::new(&mut self.font_system, metrics);
            buffer.set_size(
                &mut self.font_system,
                Some(screen.width as f32),
                Some(screen.height as f32),
            );
            buffer.set_text(&mut self.font_system, &area.text, &Attrs::new(), Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut self.font_system, false);
            self.buffers.push(buffer);
        }

        // 2) TextArea は &Buffer を借りるので、buffers Vec が安定したあとに作る
        let text_areas: Vec<TextArea<'_>> = self
            .buffers
            .iter()
            .zip(glyph_areas.iter())
            .map(|(buffer, area)| TextArea {
                buffer,
                left: area.left,
                top: area.top,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: screen.width as i32,
                    bottom: screen.height as i32,
                },
                default_color: GlyphColor::rgba(
                    (area.color.r * 255.0).clamp(0.0, 255.0) as u8,
                    (area.color.g * 255.0).clamp(0.0, 255.0) as u8,
                    (area.color.b * 255.0).clamp(0.0, 255.0) as u8,
                    (area.color.a * 255.0).clamp(0.0, 255.0) as u8,
                ),
                custom_glyphs: &[],
            })
            .collect();

        if let Err(e) = self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            eprintln!("glyph prepare error: {e:?}");
        }
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, pass) {
            eprintln!("glyph render error: {e:?}");
        }
    }
}
