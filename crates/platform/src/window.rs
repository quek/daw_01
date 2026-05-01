//! `WindowBackend` trait — winit / baseview を切り替えるための抽象。
//!
//! 上位層 (renderer, ui) はこの trait だけを介してウィンドウを扱う。
//! winit / baseview は外部 (`winit_backend`, 将来 `baseview_backend`) で実装する。

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::event::{AppEvent, PhysicalSize};

/// マウスカーソル形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorIcon {
    Default,
    Pointer,
    Text,
    Crosshair,
    /// 水平リサイズ。
    EwResize,
    /// 垂直リサイズ。
    NsResize,
    /// 移動 (4方向)。
    Move,
    /// 何も表示しない。
    Hidden,
}

/// ウィンドウバックエンドが提供する操作。
///
/// `HasWindowHandle + HasDisplayHandle` を要求することで、wgpu の `Surface` 生成に
/// 必要な raw handle を取り出せるようにする。
pub trait WindowBackend: HasWindowHandle + HasDisplayHandle {
    /// 物理ピクセル単位の現在サイズ。
    fn inner_size(&self) -> PhysicalSize;

    /// 論理↔物理スケール係数 (HiDPI)。
    fn scale_factor(&self) -> f64;

    /// 次フレームの再描画を要求。
    fn request_redraw(&self);

    /// マウスカーソル形状を変更。
    fn set_cursor(&self, cursor: CursorIcon);

    /// IME (input method editor) を有効化/無効化する。
    /// text_input が focus を取ったとき `true`、focus を失ったとき `false` を呼ぶ想定。
    fn set_ime_allowed(&self, allowed: bool);

    /// IME 候補ウィンドウを表示すべき領域 (物理ピクセル) を OS にヒントする。
    /// 一般には text_input の cursor 直下の小さな rect を渡す。
    fn set_ime_cursor_area(&self, x: f64, y: f64, w: f64, h: f64);

    /// ウィンドウタイトル更新。
    fn set_title(&self, title: &str);
}

/// アプリ実装が提供する描画/更新フック。
///
/// プラットフォーム層がイベントループを所有し、各イベントで呼び返す。
pub trait AppHost {
    /// プラットフォームから流れてきたイベントを処理。
    fn on_event(&mut self, ev: AppEvent);

    /// 描画タイミングで呼ばれる。返り値が `true` なら再描画を要求し続ける。
    fn on_render(&mut self) -> bool;
}
