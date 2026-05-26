//! glyphon 統合 — テキスト描画パイプライン。
//!
//! Buffer は (text, font_size, line_height) の hash キーで `cache` に保持し、
//! 毎フレームの再生成 (`Buffer::new` + shaping) を回避する。同一 text の繰り返し
//! 描画 (mixer の ch label 等) で大幅にコスト削減。N フレーム未使用 entry は
//! eviction (約 5 秒 @ 60fps)。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use daw_ui_platform::PhysicalSize;
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution,
    Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::MultisampleState;

/// 既定で使うフォント family。固定幅 (CJK は ASCII の 2 倍)。
/// インストールされていない環境では glyphon の fallback (システムデフォルト) に倒れる。
/// M14 Phase 58 で ui crate の `TextMetrics` が **同じフォント名で shape する** ために
/// `pub` で expose (renderer と ui の shape 設定を 1 ソースに揃える Single Source of Truth)。
pub const DEFAULT_FONT_FAMILY: &str = "HackGen Console NF";

/// この値より長く未使用の cache entry は eviction される (約 5 秒 @ 60fps)。
const EVICT_AFTER_FRAMES: u64 = 300;

use crate::scene::GlyphArea;

/// (text, font_size, line_height) を hash した cache key。
fn buffer_key(area: &GlyphArea) -> u64 {
    let mut h = DefaultHasher::new();
    area.text.hash(&mut h);
    // f32 はそのまま hash 不可なので bit 表現で。
    area.font_size.to_bits().hash(&mut h);
    area.line_height.to_bits().hash(&mut h);
    h.finish()
}

/// cache に保持する `Buffer` と、最後に使われたフレーム番号。
struct CachedBuffer {
    buffer: Buffer,
    last_seen_frame: u64,
}

/// 1 つの "run" を識別する handle。`renderers` pool 内の index を指す。
#[derive(Debug, Clone, Copy)]
pub struct GlyphRun {
    /// `renderers` pool 内の index。`u32::MAX` は empty run (描画なし)。
    idx: u32,
}

impl GlyphRun {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.idx == u32::MAX
    }
}

pub struct GlyphPipeline {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    /// 同一 (text, font_size, line_height) の Buffer を再利用する cache。
    cache: HashMap<u64, CachedBuffer>,
    /// `prepare` が呼ばれた回数。eviction 判定に使う。
    frame_counter: u64,
    /// 1 frame 内で 1 run = 1 TextRenderer。glyphon の `prepare`/`render` は 1 instance の
    /// 内部 buffer を上書きするため、複数 run を 1 instance で running するとデータが壊れる。
    /// pool は frame 内で必要数まで grow し、shrink しない (allocate コストは grow 1 度だけ)。
    renderers: Vec<TextRenderer>,
    /// 現フレームで使った run 数。`begin_frame` で 0 にリセット、`enqueue_run` で increment。
    next_renderer_idx: usize,
}

impl GlyphPipeline {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache_handle = Cache::new(device);
        let viewport = Viewport::new(device, &cache_handle);
        let mut atlas = TextAtlas::new(device, queue, &cache_handle, target_format);
        // pool の初期 instance 1 つ。frame 内で run 数が増えれば grow する。
        let initial_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
            font_system,
            swash_cache,
            viewport,
            atlas,
            cache: HashMap::new(),
            frame_counter: 0,
            renderers: vec![initial_renderer],
            next_renderer_idx: 0,
        }
    }

    /// frame 開始時に呼ぶ。pool index を 0 に戻す。viewport 更新もここで。
    pub fn begin_frame(&mut self, queue: &wgpu::Queue, screen: PhysicalSize) {
        self.frame_counter += 1;
        self.next_renderer_idx = 0;
        self.viewport.update(
            queue,
            Resolution { width: screen.width, height: screen.height },
        );
    }

    /// 1 つの run として `glyph_areas` を enqueue する。
    /// pool の index を内部で取得 (足りなければ新 `TextRenderer` を allocate)、その instance に
    /// prepare して `GlyphRun` handle を返す。`glyph_areas` が空なら empty run handle を返す。
    pub fn enqueue_run(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyph_areas: &[GlyphArea],
        screen: PhysicalSize,
    ) -> GlyphRun {
        if glyph_areas.is_empty() {
            return GlyphRun { idx: u32::MAX };
        }
        // pool grow if needed
        while self.next_renderer_idx >= self.renderers.len() {
            self.renderers.push(TextRenderer::new(
                &mut self.atlas,
                device,
                MultisampleState::default(),
                None,
            ));
        }
        let idx = self.next_renderer_idx;
        self.prepare_renderer(device, queue, idx, glyph_areas, screen);
        self.next_renderer_idx += 1;
        GlyphRun { idx: idx as u32 }
    }

    /// frame 末で呼ぶ: cache eviction を進める。
    pub fn end_frame(&mut self) {
        let frame = self.frame_counter;
        self.cache.retain(|_, e| frame.saturating_sub(e.last_seen_frame) < EVICT_AFTER_FRAMES);
    }

    /// 1 つの run を render pass に発行する。
    pub fn render_run(&self, pass: &mut wgpu::RenderPass<'_>, run: GlyphRun) {
        if run.is_empty() {
            return;
        }
        let renderer = &self.renderers[run.idx as usize];
        if let Err(e) = renderer.render(&self.atlas, &self.viewport, pass) {
            eprintln!("glyph render error: {e:?}");
        }
    }

    fn prepare_renderer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer_idx: usize,
        glyph_areas: &[GlyphArea],
        screen: PhysicalSize,
    ) {
        // 1) 各 GlyphArea のキーを計算 (2 度目の lookup を避けるため Vec に保存)。
        let keys: Vec<u64> = glyph_areas.iter().map(buffer_key).collect();

        // 2) cache に entry が無ければ新規作成、あれば last_seen_frame だけ更新。
        //    text 自体は key の一部なので、key 一致 = text 一致 → set_text 不要。
        for (area, &key) in glyph_areas.iter().zip(keys.iter()) {
            if !self.cache.contains_key(&key) {
                let metrics = Metrics::new(area.font_size, area.line_height);
                let mut buffer = Buffer::new(&mut self.font_system, metrics);
                buffer.set_size(
                    &mut self.font_system,
                    Some(screen.width as f32),
                    Some(screen.height as f32),
                );
                let attrs = Attrs::new().family(Family::Name(DEFAULT_FONT_FAMILY));
                buffer.set_text(&mut self.font_system, &area.text, &attrs, Shaping::Advanced, None);
                buffer.shape_until_scroll(&mut self.font_system, false);
                self.cache.insert(key, CachedBuffer { buffer, last_seen_frame: self.frame_counter });
            } else if let Some(entry) = self.cache.get_mut(&key) {
                entry.last_seen_frame = self.frame_counter;
            }
        }

        // 3) TextArea は &Buffer を借りるので、cache 更新後に immutable borrow で構築。
        // M7 Phase 22: GlyphArea.clip_rect が Some なら TextBounds で範囲外をクリップ。
        let text_areas: Vec<TextArea<'_>> = glyph_areas
            .iter()
            .zip(keys.iter())
            .map(|(area, &key)| {
                let bounds = area.clip_rect.map_or(
                    TextBounds {
                        left: 0,
                        top: 0,
                        right: screen.width.try_into().unwrap_or(i32::MAX),
                        bottom: screen.height.try_into().unwrap_or(i32::MAX),
                    },
                    |c| TextBounds {
                        left: c.x.max(0.0) as i32,
                        top: c.y.max(0.0) as i32,
                        right: ((c.x + c.w).min(screen.width as f32)).max(0.0) as i32,
                        bottom: ((c.y + c.h).min(screen.height as f32)).max(0.0) as i32,
                    },
                );
                TextArea {
                    buffer: &self.cache[&key].buffer,
                    left: area.left,
                    top: area.top,
                    scale: 1.0,
                    bounds,
                    default_color: GlyphColor::rgba(
                        (area.color.r * 255.0).clamp(0.0, 255.0) as u8,
                        (area.color.g * 255.0).clamp(0.0, 255.0) as u8,
                        (area.color.b * 255.0).clamp(0.0, 255.0) as u8,
                        (area.color.a * 255.0).clamp(0.0, 255.0) as u8,
                    ),
                    custom_glyphs: &[],
                }
            })
            .collect();

        if let Err(e) = self.renderers[renderer_idx].prepare(
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

    /// テスト / デバッグ用: 現在 cache に保持されている entry の数。
    #[cfg(any(test, debug_assertions))]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// M14 Phase 78 (daw_01 #049): `TextEffectCompositor` が offscreen text render で
    /// 同じ font / glyph rasterization を共有するための disjoint-field borrow accessor。
    /// `Cache` (= glyphon pipeline cache、 Arc<Inner> で cheap) は別 instance を持つが、
    /// font_system / swash_cache は重い (font 全 load + glyph raster cache) なので共有する。
    pub fn font_system_and_swash(&mut self) -> (&mut FontSystem, &mut SwashCache) {
        (&mut self.font_system, &mut self.swash_cache)
    }
}

#[cfg(test)]
mod tests {
    //! `buffer_key` の単純な動作確認 (異なる入力で異なるキー / 同じ入力で同じキー)。
    //! `prepare` の挙動はwgpu インスタンスが必要なので renderer crate 内では直接テスト
    //! できない。代わりに mixer / waveform_validation の実機検証で regression を見る。

    use super::*;
    use crate::scene::Color;

    fn area(text: &str, fs: f32, lh: f32) -> GlyphArea {
        GlyphArea {
            text: text.into(),
            left: 0.0,
            top: 0.0,
            font_size: fs,
            line_height: lh,
            color: Color::rgb(1.0, 1.0, 1.0),
            clip_rect: None,
            ..GlyphArea::default()
        }
    }

    #[test]
    fn buffer_key_same_input_same_key() {
        let a = area("hello", 14.0, 18.0);
        let b = area("hello", 14.0, 18.0);
        assert_eq!(buffer_key(&a), buffer_key(&b));
    }

    #[test]
    fn buffer_key_text_diff() {
        let a = area("hello", 14.0, 18.0);
        let b = area("world", 14.0, 18.0);
        assert_ne!(buffer_key(&a), buffer_key(&b));
    }

    #[test]
    fn buffer_key_font_size_diff() {
        let a = area("same", 14.0, 18.0);
        let b = area("same", 16.0, 18.0);
        assert_ne!(buffer_key(&a), buffer_key(&b));
    }

    #[test]
    fn buffer_key_line_height_diff() {
        let a = area("same", 14.0, 18.0);
        let b = area("same", 14.0, 20.0);
        assert_ne!(buffer_key(&a), buffer_key(&b));
    }
}
