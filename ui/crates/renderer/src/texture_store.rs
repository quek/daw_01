// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `TextureStore` — `TextureHandle` ↔ `wgpu::Texture` + `BindGroup` の管理 (M14 Phase 71 / daw_01 #043)。
//!
//! - **Renderer-local**: `Renderer<W>` / `OffscreenRenderer` 各 instance が独自の Store を持つ
//!   (= 別 Renderer 間で `TextureHandle` を共有してはならない)。
//! - **lifecycle = caller 管理**: `create` で得た handle を `upload_*` で毎フレーム更新し、
//!   不要になったら `destroy` で解放。 GUI 側 LRU 等は持たない (caller が clip-aware に
//!   ライフサイクル管理する設計、 #043 reply 参照)。
//! - **format**: M14 Phase 73 (daw_01 #045) で **per-entry format** 化。 `Rgba8UnormSrgb`
//!   (既存 #043 経路) と `Bgra8UnormSrgb` (新規 #045 経路、 CPU swap 除去) を同 store 内で混在可。
//!   sampling pipeline (`pipelines::texture`) の bind layout (`Float { filterable: true }` +
//!   `Filtering` sampler) は format 不問なので、 binding は共通で OK。 fragment 出力経路は
//!   sRGB → linear → blend → sRGB で正しく composite される (memory: wgpu 29 既知の罠ノート)。
//! - **upload 経路**: `Queue::write_texture` 直接呼び (staging buffer 不要、 wgpu の内部
//!   staging belt が 60fps 毎フレーム update を吸収)。 `bytes_per_row` の 256 倍数制約は
//!   `write_texture` には適用されない。
//! - **format mismatch on upload**: caller が `upload_with_format(handle, RGBA, ...)` を呼ぶ
//!   ところ handle 自体は BGRA で作成済 (or 逆) のとき、 **debug build では panic、 release
//!   では silent no-op** (= caller protect の defensive、 production で予期せぬ channel swap
//!   描画を起こさない方針)。

use std::collections::HashMap;
use std::num::NonZeroU32;

use crate::scene::TextureHandle;

struct TextureEntry {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// M14 Phase 73 (daw_01 #045): 1 entry = 1 format。 `Rgba8UnormSrgb` / `Bgra8UnormSrgb`
    /// が当面の値域、 将来 Phase 74 (D3D11 import) / NV12 (#046+) で追加候補。
    format: wgpu::TextureFormat,
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
        Self::new_starting_at(0)
    }

    /// `next_id` を引き継いで空の store を作る (device lost → GPU 資産再生成、 daw_01 r.md #42)。
    ///
    /// **なぜ 0 に戻してはいけないか**: `TextureHandle` は「id が生きているか」だけで判定される
    /// (destroy 済 handle への操作は no-op という契約)。 再生成で id 空間を 0 に巻き戻すと、
    /// 呼び出し側が取りこぼした **旧世代の handle が新しい別テクスチャを指す** (別名衝突) ため、
    /// upload / 描画が **無言で違う絵になる**。 build も test も clippy も全部通ってしまう類の
    /// visual regression なので、 id 空間の単調性を構造的に保つ。
    #[must_use]
    pub fn new_starting_at(next_id: u32) -> Self {
        Self {
            next_id,
            entries: HashMap::new(),
        }
    }

    /// 次に払い出す id の直前値 (= これまでに払い出した最大 id)。
    /// [`Self::new_starting_at`] に渡して世代をまたいで id 空間を継続させる。
    #[must_use]
    pub fn next_id(&self) -> u32 {
        self.next_id
    }

    /// 指定サイズ + format の空 texture を確保し、 `(texture_view, sampler)` の bind_group を
    /// `layout` で作って store する。
    ///
    /// `width` / `height` が 0 のときは 1 に clamp (wgpu が panic するため)。
    /// `format` は M14 Phase 73 (#045) 時点で `Rgba8UnormSrgb` / `Bgra8UnormSrgb` を想定。
    /// 他の format も技術的には受け付けるが、 binding layout (`Float filterable`) で sampleable
    /// である前提 (= depth / integer formats は NG)。
    pub fn create(
        &mut self,
        device: &wgpu::Device,
        sampler: &wgpu::Sampler,
        layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
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
            format,
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
                format,
            },
        );
        TextureHandle::from_raw(id)
    }

    /// `width * height * 4` bytes で texture content を上書き。 caller は `expected_format` を
    /// 渡して、 entry が同 format で作成されているかを内部 check する (= cross-format upload を
    /// 静かに描画破綻させない defensive)。
    ///
    /// - destroy 済 handle: no-op (panic しない)
    /// - size 不一致: no-op + debug_assert! (debug 時 panic)
    /// - format 不一致 (entry.format != expected_format): no-op + debug_assert! (同上)
    /// - 部分 update は MVP 非対応 (#043 reply 参照)
    pub fn upload_with_format(
        &self,
        queue: &wgpu::Queue,
        handle: TextureHandle,
        expected_format: wgpu::TextureFormat,
        data: &[u8],
    ) {
        let Some(entry) = self.entries.get(&handle.raw_id()) else {
            return;
        };
        if entry.format != expected_format {
            debug_assert_eq!(
                entry.format, expected_format,
                "upload_with_format: handle format {:?} != caller-asserted {:?}",
                entry.format, expected_format,
            );
            return;
        }
        let expected = (entry.width as usize) * (entry.height as usize) * 4;
        if data.len() != expected {
            debug_assert_eq!(
                data.len(),
                expected,
                "upload_with_format: data.len() {} != width*height*4 {}",
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

    /// M14 Phase 78 (daw_01 #049): **render target** として使える texture を作成して entry 化。
    ///
    /// 既存 `create` との違い: usage に `RENDER_ATTACHMENT` を追加 (= caller が
    /// `begin_render_pass` の `color_attachments[].view` で書き出し可能)、 `COPY_DST` は不要
    /// なので外す (CPU upload しない、 GPU 内 stay 用途)。 text effect composite (Phase 78)、
    /// post-process pass の中間 / 最終 target で使う想定。
    ///
    /// 戻り値の `TextureView` は `color_attachments` で渡すための view (caller が encoder 発行
    /// 時に使い、 use 後は drop して OK)。 sampling 用 bind_group は store 内に別 view で
    /// 保持されているので、 戻り値の view を消費しても後段 sample に影響しない。
    pub fn create_render_target(
        &mut self,
        device: &wgpu::Device,
        sampler: &wgpu::Sampler,
        layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> (TextureHandle, wgpu::TextureView) {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).expect("texture id overflow at 1");
        let w = width.max(1);
        let h = height.max(1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture pool entry (render target)"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        // sampling 用 view (store の bind_group 内、 寿命 = entry と同じ)
        let sample_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture pool bg (render target)"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&sample_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        // color_attachments 用 view (caller が use して drop する別 view)
        let target_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("texture pool render target view"),
            ..wgpu::TextureViewDescriptor::default()
        });
        self.entries.insert(
            id,
            TextureEntry {
                texture,
                bind_group,
                width: w,
                height: h,
                format,
            },
        );
        (TextureHandle::from_raw(id), target_view)
    }

    /// M14 Phase 74 (daw_01 #045 §B): 外部で構築済の `wgpu::Texture` を import して entry 化。
    /// `create` は内部で `device.create_texture(...)` を呼ぶが、 こちらは caller が既に持っている
    /// wgpu::Texture (e.g. `wgpu::Device::create_texture_from_hal` で D3D11 shared handle から構築
    /// 済の texture) を所有移譲で受け取り、 同じ bind_group / format / size 管理に乗せる。
    ///
    /// `width` / `height` / `format` は caller が知っている前提で渡す (= imported texture を
    /// `wgpu::Texture::format()` / `size()` 等で取り出すこともできるが、 caller 側で
    /// 既に shared handle import 時に把握しているので boilerplate を避けて引数受けにする)。
    ///
    /// 引数 8 件は wgpu init で得る不可分の resource bundle (device / sampler / layout) +
    /// imported texture + 3 metadata から成り、 struct 化しても caller 側で構築 boilerplate が
    /// 増えるだけなので flat 受けのまま `#[allow(too_many_arguments)]` で抑止する。
    #[allow(clippy::too_many_arguments)]
    pub fn import_texture(
        &mut self,
        device: &wgpu::Device,
        sampler: &wgpu::Sampler,
        layout: &wgpu::BindGroupLayout,
        texture: wgpu::Texture,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> TextureHandle {
        self.next_id += 1;
        let id = NonZeroU32::new(self.next_id).expect("texture id overflow at 1");
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture pool bg (imported)"),
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
                width: width.max(1),
                height: height.max(1),
                format,
            },
        );
        TextureHandle::from_raw(id)
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

    /// M14 Phase 73 (#045): entry の format を返す。 destroy 済は `None`。
    /// debug / test 用 (`Renderer::texture_size` と対の helper)、 描画 path では使わない
    /// (sampling は bind_group 経由で format-agnostic)。
    #[must_use]
    pub fn format(&self, handle: TextureHandle) -> Option<wgpu::TextureFormat> {
        self.entries.get(&handle.raw_id()).map(|e| e.format)
    }

    /// render pipeline の `set_bind_group(1, ..)` で参照する bind_group。
    /// destroy 済は `None` (caller が draw call を skip)。
    #[must_use]
    pub fn bind_group(&self, handle: TextureHandle) -> Option<&wgpu::BindGroup> {
        self.entries.get(&handle.raw_id()).map(|e| &e.bind_group)
    }

    /// M14 Phase 78 (daw_01 #049): handle から `&wgpu::Texture` を取り出す。 text_effect の
    /// blur / composite pass で独自 BindGroupLayout (= 2 texture binding 等) を新規 bind_group
    /// するため、 source texture へのアクセスが必要 (= 既存 `bind_group()` は texture pipeline
    /// 専用 layout 用なので、 異なる layout に対しては再 bind 必要)。
    #[must_use]
    pub fn raw_texture(&self, handle: TextureHandle) -> Option<&wgpu::Texture> {
        self.entries.get(&handle.raw_id()).map(|e| &e.texture)
    }
}

impl Default for TextureStore {
    fn default() -> Self {
        Self::new()
    }
}
