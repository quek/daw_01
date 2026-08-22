//! `WindowBackend` trait — winit / baseview を切り替えるための抽象。
//!
//! 上位層 (renderer, ui) はこの trait だけを介してウィンドウを扱う。
//! winit / baseview は外部 (`winit_backend`, 将来 `baseview_backend`) で実装する。

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::event::{AppEvent, PhysicalSize};
use crate::text_document::{ImeTextEdit, TextDocument};

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
///
/// # プラグイン UI 埋め込み (DAW プラグイン用)
///
/// 外部 crate でも `WindowBackend` を `impl` できる。VST3 / CLAP プラグインで親
/// アプリから受け取った raw window handle を保持する型に対し、`HasWindowHandle` /
/// `HasDisplayHandle` / `WindowBackend` を実装すれば、`daw_ui_renderer::Renderer<W>`
/// にそのまま渡せる。実装例は `examples/embedded_host` を参照。
///
/// プラグイン host が `on_frame` 等で frame push する場合は、
/// [`drive_one_frame`](crate::winit_backend::drive_one_frame) を host 側ループから
/// 呼べば、winit と同じ frame driver で動く。
pub trait WindowBackend: HasWindowHandle + HasDisplayHandle {
    /// 物理ピクセル単位の現在サイズ。
    fn inner_size(&self) -> PhysicalSize;

    /// 論理↔物理スケール係数 (HiDPI)。
    fn scale_factor(&self) -> f64;

    /// 次フレームの再描画を要求。
    fn request_redraw(&self);

    /// マウスカーソル形状を変更。
    fn set_cursor(&self, cursor: CursorIcon);

    /// マウスカーソルを物理ピクセル位置へワープする (ウィンドウ client 座標)。
    /// ノート作成ドラッグでカーソルをノート右端へ移動する等に使う (Ableton Live 流)。
    /// 既定実装は no-op (embedded host / 非 winit backend / 未対応プラットフォーム)。
    fn set_cursor_position(&self, _x: f32, _y: f32) {}

    /// IME (input method editor) を有効化/無効化する。
    /// text_input が focus を取ったとき `true`、focus を失ったとき `false` を呼ぶ想定。
    fn set_ime_allowed(&self, allowed: bool);

    /// IME 候補ウィンドウを表示すべき領域 (物理ピクセル) を OS にヒントする。
    /// 一般には text_input の cursor 直下の小さな rect を渡す。
    fn set_ime_cursor_area(&self, x: f64, y: f64, w: f64, h: f64);

    /// (Windows TSF) focus 中の編集可能テキストの snapshot を OS text store に publish する。
    ///
    /// `Some(doc)` で内容/選択/caret を更新し、`None` で「編集対象なし」(text store を空にして
    /// IME を非アクティブ化) を表す。`frame()` 末尾に毎フレーム呼ばれる想定。
    /// これにより rtry (Try-Code TIP) のまぜ書き / ストロークヘルプや MS-IME 再変換が、
    /// アプリの text store から `GetText` でカーソル前テキストを読めるようになる。
    ///
    /// 既定実装は no-op (TSF 非対応プラットフォーム / 埋め込み host)。Windows の `WinitWindow`
    /// だけが TSF `ITextStoreACP` を駆動する。
    fn set_text_input_document(&self, _doc: Option<&TextDocument>) {}

    /// (Windows TSF) OS IME がこのフレームに text store へ加えた編集 (まぜ書き変換結果 /
    /// 再変換 / composition 確定) を取り出す。取り出すと内部キューは空になる。
    ///
    /// 既定実装は空 Vec を返す (編集なし)。
    fn take_ime_text_edits(&self) -> Vec<ImeTextEdit> {
        Vec::new()
    }

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
