//! 中立イベント型。winit / baseview のどちらでも、外部プラットフォーム層がここに変換する。

use std::time::Duration;

/// 物理ピクセル単位のサイズ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// 物理ピクセル単位の座標 (左上原点, +x 右, +y 下)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicalPosition {
    pub x: f64,
    pub y: f64,
}

/// マウスボタン。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u16),
}

/// ボタンの押下/解放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementState {
    Pressed,
    Released,
}

/// キーボード修飾キーの状態 (winit 非依存の中立型)。
///
/// 4 フラグは `Ctrl/Shift/Alt/Logo` の canonical な組合せ (winit / NSEvent /
/// XKB と同形)。`clippy::struct_excessive_bools` はこの種の "正規 4 つ並び" を
/// 否定する意図ではないので allow する。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Win / Cmd キー。
    pub logo: bool,
}

/// キー入力 (M1 では最低限。論理キー名は後で拡張)。
#[derive(Debug, Clone)]
pub struct KeyEvent {
    pub state: ElementState,
    /// 文字入力可能な場合の Unicode 文字列 (IME 経由含む)。
    pub text: Option<String>,
    /// 物理キーの識別子 (winit `KeyCode` 由来)。
    pub physical_key: PhysicalKey,
}

/// 物理キー (M1 ではよく使うものだけ)。後で網羅する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalKey {
    Escape,
    Enter,
    Space,
    Tab,
    Backspace,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Other(u32),
}

/// スクロール量。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDelta {
    /// マウスホイール (lines / 行単位)。
    Lines { x: f32, y: f32 },
    /// トラックパッド等 (物理ピクセル)。
    Pixels { x: f64, y: f64 },
}

/// アプリ層に流す中立イベント。
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// ウィンドウサイズが変わった (HiDPI スケール込みの物理サイズ)。
    Resized(PhysicalSize),
    /// 論理↔物理スケール係数が変わった。
    ScaleFactorChanged(f64),
    /// マウス移動。
    PointerMoved(PhysicalPosition),
    /// マウスボタン押下/解放。
    PointerInput {
        button: MouseButton,
        state: ElementState,
    },
    /// マウスがウィンドウに入った/出た。
    PointerEntered,
    PointerLeft,
    /// スクロール。
    Scroll(ScrollDelta),
    /// キー入力。
    Keyboard(KeyEvent),
    /// キーボード修飾キー (Ctrl/Shift/Alt/Logo) の状態が変わった。
    /// `MouseInput` イベントより先に届く前提で、`InputAccumulator` が単独で track する。
    ModifiersChanged(Modifiers),
    /// IME プリエディット (将来用、M1 では未使用)。
    ImePreedit { text: String, cursor: Option<(usize, usize)> },
    /// IME 確定。
    ImeCommit(String),
    /// フォーカス変更。
    Focus(bool),
    /// 描画要求 (vsync / OS 起因)。
    Redraw,
    /// 経過時間 (アニメーション等で使う)。
    Tick(Duration),
    /// ウィンドウクローズ要求。
    CloseRequested,
}
