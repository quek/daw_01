//! wgpu パイプライン群。
//!
//! M1 で必要なもの: rect (instanced 角丸矩形), glyph (テキスト)。
//! M4 以降で line (波形/メータ), quad (textured) を追加。

pub mod glyph;
pub mod rect;
