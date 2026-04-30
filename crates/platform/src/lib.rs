//! プラットフォーム層 — ウィンドウとイベントの中立抽象。
//!
//! 設計目的:
//! - winit / baseview の両方に乗れるよう、`WindowBackend` trait と中立な `AppEvent`
//!   enum を提供し、上位層 (renderer / ui) が winit の型を直接知らなくて済むようにする。
//! - raw-window-handle 経由で wgpu と接続するため、surface 作成に必要な情報を露出する。

pub mod event;
pub mod window;
pub mod winit_backend;

pub use event::*;
pub use window::*;
