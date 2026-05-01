//! wgpu パイプライン群。
//!
//! M1: rect (instanced 角丸矩形), glyph (テキスト)。
//! M2: line (波形/メータ/グリッド)。
//! M5+ で quad (textured) を追加予定。

pub mod glyph;
pub mod line;
pub mod rect;
