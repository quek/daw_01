// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 中立イベント型。winit / baseview のどちらでも、外部プラットフォーム層がここに変換する。

use std::path::PathBuf;
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
    /// サイドボタン「戻る」 (E3 / r.md #8: 旧実装は Forward と衝突し Other(0xffff))。
    Back,
    /// サイドボタン「進む」。
    Forward,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Win / Cmd キー。
    pub logo: bool,
}

impl Modifiers {
    /// すべての修飾キーが押されていない状態 (`Modifiers::default()` と同等の const 構築)。
    #[must_use]
    pub const fn empty() -> Self {
        Self { ctrl: false, shift: false, alt: false, logo: false }
    }

    /// すべての修飾キーが false なら true。
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.ctrl && !self.shift && !self.alt && !self.logo
    }

    /// `target` と完全一致するか (M8 Phase 30 shortcut マッチ用)。
    /// `Eq` で同等だが、意図が伝わるよう専用メソッドを切る。
    #[must_use]
    pub fn matches(self, target: Modifiers) -> bool {
        self == target
    }
}

/// キー入力 (M1 では最低限。論理キー名は後で拡張)。
#[derive(Debug, Clone)]
pub struct KeyEvent {
    pub state: ElementState,
    /// 文字入力可能な場合の Unicode 文字列 (IME 経由含む)。
    pub text: Option<String>,
    /// 物理キーの識別子 (winit `KeyCode` 由来)。
    pub physical_key: PhysicalKey,
    /// OS の auto-repeat (押しっぱなしで繰り返し届く) 由来か (winit `KeyEvent::repeat`)。
    ///
    /// **テキスト入力は repeat を消費する** (Backspace / 矢印の長押しが効かないと使い物に
    /// ならない) が、**global shortcut は立ち上がり 1 回だけ発火させる**。 shortcut は
    /// Delete / D / E のような離散コマンドに bind されており、 repeat で連射されると
    /// 「Delete 長押しでトラックが次々消える」 のような破壊的挙動になる。 従って
    /// repeat の抑止は `Ui::frame` の shortcut 解決層だけで行い、 event 自体は
    /// `keyboard_events` に残して focused widget へ渡す。
    pub repeat: bool,
}

/// 物理キー。
///
/// M1: 制御キー / arrow / Other(u32) のみ。
/// M8 Phase 30: shortcut 解釈のため `Char(char)` (Latin alphabet 大文字)、`Digit(u8)`、
/// `F(u8)`、`Delete / Home / End / PageUp / PageDown / Insert` を追加。
/// M9 P0-1: `Char(char)` の domain を ASCII 印字可能記号 11 種にも拡張
/// (`/ ; , . - = [ ] \ ' ``)。
/// M14 Phase 57 (daw_01 #016): テンキー Enter (`NumpadEnter`) を追加。DAW 数値入力で
/// 多用される (Cubase / REAPER / Logic 等の業界慣習)。`text_input` の commit 判定で
/// `Enter | NumpadEnter` のどちらでも成立するように拡張する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalKey {
    Escape,
    Enter,
    /// テンキー (numpad) の Enter キー。`Enter` とは別の物理キーだが、
    /// commit / 改行 / shortcut 等の semantic では通常同じ扱いをする。
    NumpadEnter,
    Space,
    Tab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    /// ASCII 印字可能キー。
    /// - Latin alphabet: 大文字に正規化 ('A'..='Z')
    /// - 記号 11 種 (US 配列、shift なし時の char):
    ///   `/`, `;`, `,`, `.`, `-`, `=`, `[`, `]`, `\`, `'`, `` ` ``
    Char(char),
    /// 数字キー (上段の Digit0..=Digit9、テンキーは含まない)。
    Digit(u8),
    /// ファンクションキー (F1..=F24)。
    F(u8),
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
    /// M8 Phase 32: OS から file が hover に入った (drop 候補表示用)。
    /// 連続して同じ window 内で複数回 hover されると累積的に積まれる。winit は file を 1 つずつ
    /// 通知してくる仕様だが、`InputAccumulator` 側でフレーム単位にまとめる。
    FileHovered(PathBuf),
    /// M8 Phase 32: hover が cancel された (ドラッグ中に枠外に出た / Esc 等)。
    /// hover 累積はクリアする。
    FileHoverCancelled,
    /// M8 Phase 32: OS から file がドロップされた。
    /// winit は drop 時の cursor 位置を提供しないため、`InputAccumulator` が直近 hover の
    /// 最終 cursor 位置を覚えて同梱する (= 「audio file を timeline の N 小節目にドロップ」UX)。
    FileDropped(PathBuf),
}
