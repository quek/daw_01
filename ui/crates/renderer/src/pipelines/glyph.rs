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
    Attrs, Buffer, Cache, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::MultisampleState;

use crate::fonts::FontAssets;

/// 既定で使うフォント family。固定幅 (CJK は ASCII の 2 倍)。
/// インストールされていない環境では glyphon の fallback (システムデフォルト) に倒れる。
/// M14 Phase 58 で ui crate の `TextMetrics` が **同じフォント名で shape する** ために
/// `pub` で expose (renderer と ui の shape 設定を 1 ソースに揃える Single Source of Truth)。
pub const DEFAULT_FONT_FAMILY: &str = "HackGen Console NF";

/// この値より長く未使用の cache entry は eviction される (約 5 秒 @ 60fps)。
const EVICT_AFTER_FRAMES: u64 = 300;

use crate::scene::GlyphArea;

/// (text, font_size, line_height, font, wrap size) を hash した cache key。
fn buffer_key(area: &GlyphArea, wrap_w: f32, wrap_h: f32) -> u64 {
    let mut h = DefaultHasher::new();
    area.text.hash(&mut h);
    // f32 はそのまま hash 不可なので bit 表現で。
    area.font_size.to_bits().hash(&mut h);
    area.line_height.to_bits().hash(&mut h);
    // M14 Phase 121 (daw_01 #096): font も cache identity の一部。 同 text+size でも font 違いは
    // 別 buffer にしないと、 先に焼いた font の buffer が後の area に化ける (cache collision)。
    area.resolved_font_family().hash(&mut h);
    // (review) 折り返し幅 (`set_size` に渡す screen size) も identity の一部。
    // 抜けると window resize 後も旧幅で折り返し続け、 composite pass (group
    // サイズを screen として渡す) と base pass (surface サイズ) が同一 buffer を
    // 共有して stale な wrap レイアウトになる。 旧サイズの entry は eviction で
    // 自然回収される。
    wrap_w.to_bits().hash(&mut h);
    wrap_h.to_bits().hash(&mut h);
    h.finish()
}

/// cache に保持する `Buffer` と、最後に使われたフレーム番号。
struct CachedBuffer {
    buffer: Buffer,
    last_seen_frame: u64,
}

/// 既に shape 済の glyphon `Buffer` から描画ブロックの実寸 `(最大行 advance, 総高さ)` を測る。
///
/// M14 Phase 122 (daw_01 #097): box align の基準。 非 effect path (`prepare_renderer`) と
/// effect path (`text_effect::measure_text`) の **双方がこの 1 関数を読む** ことで measure を
/// SSoT 化する (= 同じ font / shaping 設定なら plain と offscreen で advance が一致)。
///
/// 戻り値は **渡された buffer 自身の wrap 設定での実寸**。 各 area は `has_effects()` で plain か
/// effect の片方の path しか通らないので、 「plain は screen 幅で wrap / effect は no-wrap」 という
/// buffer 構築差が同一 area で衝突することは無く、 各 path 内で「描画される block を測る」 ので
/// align は常に実描画と一致する (overlay title は単一行 < screen 幅で wrap しない想定)。
pub(crate) fn measure_layout(buffer: &Buffer) -> (f32, f32) {
    let mut max_w: f32 = 0.0;
    let mut total_h: f32 = 0.0;
    for run in buffer.layout_runs() {
        max_w = max_w.max(run.line_w);
        total_h += run.line_height;
    }
    (max_w, total_h)
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

/// テキスト描画の **GPU 側**資産のみを持つ pipeline。
///
/// font DB / glyph raster cache は GPU に依存しない CPU 資産なので
/// [`FontAssets`](crate::fonts::FontAssets) 側が所有し、 `enqueue_run` に `&mut` で渡される
/// (device lost で GPU 資産だけ捨てて作り直せるようにするための分離、 daw_01 r.md #42)。
pub struct GlyphPipeline {
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
        let cache_handle = Cache::new(device);
        let viewport = Viewport::new(device, &cache_handle);
        let mut atlas = TextAtlas::new(device, queue, &cache_handle, target_format);
        // pool の初期 instance 1 つ。frame 内で run 数が増えれば grow する。
        let initial_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
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
        fonts: &mut FontAssets,
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
        self.prepare_renderer(device, queue, fonts, idx, glyph_areas, screen);
        self.next_renderer_idx += 1;
        GlyphRun { idx: idx as u32 }
    }

    /// frame 末で呼ぶ: layout buffer cache eviction を進め、 glyph atlas を trim する。
    ///
    /// # なぜ `atlas.trim()` が要るか (daw_01 r.md #59)
    ///
    /// glyphon の `TextAtlas` は **毎フレーム `trim()` される前提** で設計されている
    /// (upstream `examples/hello-world.rs` / `text-sizes.rs` / `custom-glyphs.rs` /
    /// `benches/prepare.rs` がいずれも submit 後に呼ぶ)。 `TextRenderer::prepare` は描いた
    /// glyph の cache key を `InnerAtlas::glyphs_in_use` に挿し、 `trim()` だけがそれを
    /// clear する。 呼ばないと **一度描いた全 glyph が永久に in-use** になり、
    /// `InnerAtlas::try_allocate` の LRU が 1 つも追い出せず `None` を返す → glyphon は
    /// atlas texture を 2 倍に `grow` する。 256 → 512 → … と育ち、 上限は
    /// `device.limits().max_texture_dimension_2d` = **8192**
    /// (`Renderer` / `OffscreenRenderer` とも `required_limits: wgpu::Limits::default()` で
    /// 生成しており、 wgpu 29 の default は 8192)。 到達すると mask atlas (R8) 1 枚で 64 MiB、
    /// color 絵文字を描けば color atlas (Rgba8) が 256 MiB。 しかも atlas は
    /// **GlyphPipeline (base) / GlyphPipeline (popup) / TextEffectCompositor** に 1 枚ずつ、
    /// さらに preview 窓と export 用 `OffscreenRenderer` が独自 renderer なので別系統に増える。
    /// 8192 に達した後は `grow` が false を返して `PrepareError::AtlasFull` になり、
    /// 今度は **文字が描画されなくなる**。
    ///
    /// font size は zoom / トラック高さから連続的に決まり (piano_roll の歌詞は `note_h * 0.75`、
    /// 字幕は preview 窓幅 / project 幅のスケール)、 cosmic-text の `CacheKey` は font size を
    /// 生ビットで持つので、 縦スクロールや窓リサイズだけで key が無限に増える = 実運用で必ず踏む。
    ///
    /// # なぜ submit 前のこの位置で安全か
    ///
    /// `trim()` が触るのは CPU 側 `HashSet` のみで GPU 資産は動かない。 実際の eviction は
    /// **次の** `prepare` が packer を埋めたときに起き、 その上書きは `queue.write_texture`
    /// = 次 submit の先頭で実行される。 `end_frame` は当フレームの全 `prepare` が済んだ後
    /// (= 以後 prepare されない) に呼ばれるので、 encode 済み draw が参照する atlas 領域が
    /// 同一 submit 内で書き換わることはない。
    pub fn end_frame(&mut self) {
        let frame = self.frame_counter;
        self.cache.retain(|_, e| frame.saturating_sub(e.last_seen_frame) < EVICT_AFTER_FRAMES);
        self.atlas.trim();
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

    #[allow(clippy::too_many_arguments)]
    fn prepare_renderer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fonts: &mut FontAssets,
        renderer_idx: usize,
        glyph_areas: &[GlyphArea],
        screen: PhysicalSize,
    ) {
        // 1) 各 GlyphArea のキーを計算 (2 度目の lookup を避けるため Vec に保存)。
        let keys: Vec<u64> = glyph_areas
            .iter()
            .map(|a| buffer_key(a, screen.width as f32, screen.height as f32))
            .collect();

        // 2) cache に entry が無ければ新規作成、あれば last_seen_frame だけ更新。
        //    text 自体は key の一部なので、key 一致 = text 一致 → set_text 不要。
        for (area, &key) in glyph_areas.iter().zip(keys.iter()) {
            if !self.cache.contains_key(&key) {
                let metrics = Metrics::new(area.font_size, area.line_height);
                let fs = &mut fonts.font_system;
                let mut buffer = Buffer::new(fs, metrics);
                buffer.set_size(fs, Some(screen.width as f32), Some(screen.height as f32));
                let attrs = Attrs::new().family(Family::Name(area.resolved_font_family()));
                buffer.set_text(fs, &area.text, &attrs, Shaping::Advanced, None);
                buffer.shape_until_scroll(fs, false);
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
                // 丸め規則は rect / line / texture と同じ `scissor_edges` に一本化する
                // (旧実装だけ各辺独立 floor で、 同じ論理 clip でもグリフだけ 1px ずれた)。
                let full = TextBounds {
                    left: 0,
                    top: 0,
                    right: screen.width.try_into().unwrap_or(i32::MAX),
                    bottom: screen.height.try_into().unwrap_or(i32::MAX),
                };
                let bounds = area
                    .clip_rect
                    .map_or(full, |c| {
                        super::scissor::scissor_edges(c, screen).map_or(
                            // 空 clip = 何も描かない。 潰れた bounds を渡して抑止する。
                            TextBounds { left: 0, top: 0, right: 0, bottom: 0 },
                            |[l, t, r, b]| TextBounds {
                                left: i32::try_from(l).unwrap_or(i32::MAX),
                                top: i32::try_from(t).unwrap_or(i32::MAX),
                                right: i32::try_from(r).unwrap_or(i32::MAX),
                                bottom: i32::try_from(b).unwrap_or(i32::MAX),
                            },
                        )
                    });
                // M14 Phase 122 (daw_01 #097): box + 非 default align のときだけ実測 advance を
                // 測って描画原点を補正。 box 無し / Left+Top は measure を skip して既存挙動。
                let (left, top) = if area.needs_alignment() {
                    let (tw, th) = measure_layout(&self.cache[&key].buffer);
                    area.aligned_origin(tw, th)
                } else {
                    (area.left, area.top)
                };
                TextArea {
                    buffer: &self.cache[&key].buffer,
                    left,
                    top,
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

        let (font_system, swash_cache) = fonts.split();
        if let Err(e) = self.renderers[renderer_idx].prepare(
            device,
            queue,
            font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            swash_cache,
        ) {
            eprintln!("glyph prepare error: {e:?}");
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
        assert_eq!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
    }

    #[test]
    fn buffer_key_text_diff() {
        let a = area("hello", 14.0, 18.0);
        let b = area("world", 14.0, 18.0);
        assert_ne!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
    }

    #[test]
    fn buffer_key_font_size_diff() {
        let a = area("same", 14.0, 18.0);
        let b = area("same", 16.0, 18.0);
        assert_ne!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
    }

    #[test]
    fn buffer_key_line_height_diff() {
        let a = area("same", 14.0, 18.0);
        let b = area("same", 14.0, 20.0);
        assert_ne!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
    }

    /// M14 Phase 121 (daw_01 #096): 同 text+size+line_height でも font 違いは別 key
    /// (= 別 buffer)。 これが無いと cache collision で別 font に化ける。
    #[test]
    fn buffer_key_font_family_diff() {
        let mut a = area("same", 14.0, 18.0);
        let mut b = area("same", 14.0, 18.0);
        a.font_family = Some("Arial".into());
        b.font_family = Some("Times New Roman".into());
        assert_ne!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
    }

    /// `None` と `Some(DEFAULT_FONT_FAMILY)` は同じ font に解決されるので **同じ key** にする
    /// (resolved_font_family 経由なので cache を無駄に二重化しない)。
    #[test]
    fn buffer_key_none_eq_default_name() {
        let a = area("same", 14.0, 18.0); // font_family: None
        let mut b = area("same", 14.0, 18.0);
        b.font_family = Some(DEFAULT_FONT_FAMILY.into());
        assert_eq!(buffer_key(&a, 800.0, 600.0), buffer_key(&b, 800.0, 600.0));
        // `Some("")` も default 扱い (daw_01 の "" = default 慣習)。
        let mut c = area("same", 14.0, 18.0);
        c.font_family = Some("".into());
        assert_eq!(buffer_key(&a, 800.0, 600.0), buffer_key(&c, 800.0, 600.0));
    }
}
