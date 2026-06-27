//! レンダラ — wgpu の上に組む自前パイプライン群。
//!
//! 提供:
//! - `Renderer`: wgpu デバイス・キュー・サーフェス・パイプラインを束ねた高レベル入口
//! - `pipelines::rect`: instanced 角丸矩形パイプライン (ボタン・ノブ・ノート等)
//! - `pipelines::texture`: textured quad (video frame / thumbnail、 M14 Phase 71)
//! - `pipelines::line`: line strip (波形・メータ・グリッド)
//! - `pipelines::glyph`: glyphon 統合 (テキスト)
//!
//! シーンは内部的に DisplayList として保持し、フレーム終端でバッチ描画する。

mod composite;
pub mod device;
pub mod fonts;
pub mod offscreen;
pub mod pipelines;
pub mod scene;
pub mod texture_store;
pub mod theme;

// M14 Phase 133 (daw_01 #111): 公開 API (`Renderer::raw_texture` / `create_render_target` /
// `device` / `queue` 等) が `wgpu` 型を直接やり取りするので、 caller (daw_01 映像効果) が
// **gui_01 と完全に同一 version の wgpu** を名指しできるよう re-export する (version drift で型が
// 不一致になる boilerplate を caller に強要しない、 [[feedback_pursue_best_practice]])。
pub use wgpu;

pub use device::*;
pub use fonts::available_font_families;
pub use offscreen::*;
pub use pipelines::glyph::DEFAULT_FONT_FAMILY;
pub use scene::*;
