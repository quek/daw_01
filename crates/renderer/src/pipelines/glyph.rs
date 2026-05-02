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

/// 既定で使うフォント family。固定幅 (CJK は ASCII の 2 倍) なので、
/// `text_input` などの cursor / 下線位置を ASCII=font_size/2、CJK=font_size の
/// 単純な近似で正しく出せる。インストールされていない環境では glyphon の
/// fallback (システムデフォルト) に倒れる。
const DEFAULT_FONT_FAMILY: &str = "HackGen Console NF";

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

pub struct GlyphPipeline {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// 同一 (text, font_size, line_height) の Buffer を再利用する cache。
    cache: HashMap<u64, CachedBuffer>,
    /// `prepare` が呼ばれた回数。eviction 判定に使う。
    frame_counter: u64,
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
            cache: HashMap::new(),
            frame_counter: 0,
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyph_areas: &[GlyphArea],
        screen: PhysicalSize,
    ) {
        self.frame_counter += 1;
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
            // eviction は空フレームでも進めて、長時間 idle で残骸を残さない。
            let frame = self.frame_counter;
            self.cache.retain(|_, e| frame.saturating_sub(e.last_seen_frame) < EVICT_AFTER_FRAMES);
            return;
        }

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

        // 4) 古い entry の eviction。EVICT_AFTER_FRAMES より長く使われていない entry は捨てる。
        let frame = self.frame_counter;
        self.cache.retain(|_, e| frame.saturating_sub(e.last_seen_frame) < EVICT_AFTER_FRAMES);
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, pass) {
            eprintln!("glyph render error: {e:?}");
        }
    }

    /// テスト / デバッグ用: 現在 cache に保持されている entry の数。
    #[cfg(any(test, debug_assertions))]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
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
            text: text.to_string(),
            left: 0.0,
            top: 0.0,
            font_size: fs,
            line_height: lh,
            color: Color::rgb(1.0, 1.0, 1.0),
            clip_rect: None,
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
