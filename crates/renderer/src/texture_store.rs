//! `TextureStore` — `TextureHandle` ↔ `wgpu::Texture` + `BindGroup` の管理 (M14 Phase 71 / daw_01 #043)。
//!
//! - **Renderer-local**: `Renderer<W>` / `OffscreenRenderer` 各 instance が独自の Store を持つ
//!   (= 別 Renderer 間で `TextureHandle` を共有してはならない)。
//! - **lifecycle = caller 管理**: `create_texture` で得た handle を `upload_texture_rgba` で
//!   毎フレーム更新し、 不要になったら `destroy_texture` で解放。 GUI 側 LRU 等は持たない
//!   (caller が clip-aware にライフサイクル管理する設計、 #043 reply 参照)。
//! - **format 固定**: `Rgba8UnormSrgb`。 fragment 出力が sRGB → linear → blend → sRGB の
//!   正しいパスで composite される (memory: project_overview.md / wgpu 29 既知の罠ノート)。
//! - **upload 経路**: `Queue::write_texture` 直接呼び (staging buffer 不要、 wgpu の内部
//!   staging belt が 60fps 毎フレーム update を吸収)。 `bytes_per_row` の 256 倍数制約は
//!   `write_texture` には適用されない。

use std::collections::HashMap;
use std::num::NonZeroU32;

use crate::scene::TextureHandle;

struct TextureEntry {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

/// `TextureHandle` → `wgpu::Texture` + `BindGroup` の lookup table。
///
/// `next_id` は単調増加 (destroy で空いた id は再利用しない、 use-after-destroy 検出を簡略化)。
pub struct TextureStore {
    next_id: u32,
    entries: HashMap<NonZeroU32, TextureEntry>,
}

impl TextureStore {
    pub fn new() -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
        }
    }

    /// 指定サイズの空 RGBA8UnormSrgb texture を確保し、 `(texture_view, sampler)` の
    /// bind_group を `layout` で作って store する。
    ///
    /// `width` / `height` が 0 のときは 1 に clamp (wgpu が panic するため)。
    pub fn create(
        &mut self,
        device: &wgpu::Device,
        sampler: &wgpu::Sampler,
        layout: &wgpu::BindGroupLayout,
        width: u32,
        height: u32,
    ) -> TextureHandle {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).expect("texture id overflow at 1");
        let w = width.max(1);
        let h = height.max(1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture pool entry"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture pool bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        self.entries.insert(
            id,
            TextureEntry {
                texture,
                bind_group,
                width: w,
                height: h,
            },
        );
        TextureHandle::from_raw(id)
    }

    /// RGBA8 (= `width * height * 4` bytes) で texture content を上書き。
    ///
    /// - destroy 済 handle / size 不一致は no-op (panic しない)
    /// - 部分 update は MVP 非対応 (#043 reply 参照)
    pub fn upload(&self, queue: &wgpu::Queue, handle: TextureHandle, data: &[u8]) {
        let Some(entry) = self.entries.get(&handle.raw_id()) else {
            return;
        };
        let expected = (entry.width as usize) * (entry.height as usize) * 4;
        if data.len() != expected {
            // size 不一致はサイレント no-op。 debug build では panic させたいなら debug_assert を入れる。
            debug_assert_eq!(
                data.len(),
                expected,
                "upload_texture_rgba: data.len() {} != width*height*4 {}",
                data.len(),
                expected,
            );
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &entry.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(entry.width * 4),
                rows_per_image: Some(entry.height),
            },
            wgpu::Extent3d {
                width: entry.width,
                height: entry.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// handle を解放。 既に解放済 / 未知の handle は no-op。 以後 `bind_group` / `size` は `None`。
    pub fn destroy(&mut self, handle: TextureHandle) {
        self.entries.remove(&handle.raw_id());
    }

    /// texture の native (width, height) を返す。 destroy 済は `None`。
    /// arrangement clip thumbnail の aspect-fit 計算 (#044) で widget 内部から参照される。
    #[must_use]
    pub fn size(&self, handle: TextureHandle) -> Option<(u32, u32)> {
        self.entries.get(&handle.raw_id()).map(|e| (e.width, e.height))
    }

    /// render pipeline の `set_bind_group(1, ..)` で参照する bind_group。
    /// destroy 済は `None` (caller が draw call を skip)。
    #[must_use]
    pub fn bind_group(&self, handle: TextureHandle) -> Option<&wgpu::BindGroup> {
        self.entries.get(&handle.raw_id()).map(|e| &e.bind_group)
    }
}

impl Default for TextureStore {
    fn default() -> Self {
        Self::new()
    }
}
