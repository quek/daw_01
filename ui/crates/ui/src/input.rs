// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 入力状態 — `AppEvent` を蓄積してフレーム毎に Ui に渡す形にする。

use std::path::PathBuf;

use daw_ui_platform::{AppEvent, ElementState, KeyEvent, Modifiers, MouseButton, PhysicalPosition, ScrollDelta};

/// マウスホイール 1 line を何 px に換算するか (Windows / Linux / macOS で慣用される値)。
const LINE_HEIGHT_PX: f32 = 40.0;

/// M8 Phase 32: OS から drop された file 群と drop 直前の cursor 座標。
///
/// `PointerFrame` は `Copy` を維持するため、`Vec<PathBuf>` を含むこちらは別 field
/// (`FrameInput::file_drop` / `Ui::file_drop`) として扱う。
#[derive(Debug, Clone, Default)]
pub struct DroppedFiles {
    pub paths: Vec<PathBuf>,
    pub position: (f32, f32),
}

/// 1 フレーム分のポインタ入力スナップショット。
///
/// 4 つの bool flag (primary press/release + secondary press/release) は canonical な
/// pointer event 表現で、`Modifiers` と同じく否定する意図ではないので allow する。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerFrame {
    pub pos: Option<(f32, f32)>,
    /// このフレーム内で「押下された」(pressed transition があった)。
    pub primary_just_pressed: bool,
    /// このフレーム内で「離された」(released transition があった)。
    pub primary_just_released: bool,
    /// 現在押下中 (左ボタン)。
    pub primary_pressed: bool,
    /// このフレーム内で右ボタンが押下された (context menu トリガ用)。
    pub secondary_just_pressed: bool,
    /// このフレーム内で右ボタンが離された。
    pub secondary_just_released: bool,
    /// 現在の修飾キー状態 (Ctrl / Shift / Alt / Logo)。Ctrl+drag による高精度モードなどに使う。
    pub modifiers: Modifiers,
    /// このフレーム内に蓄積されたスクロール量 (物理 px)。
    /// 符号は winit の慣行に従い「`y > 0` = wheel を上方向に回した = コンテンツが上に流れる」。
    /// scroll_area は `state.offset.y -= scroll_delta.1` のように offset を更新する。
    pub scroll_delta: (f32, f32),
}

/// IME (input method editor) のイベント。focused widget が処理する。
#[derive(Debug, Clone)]
pub enum ImeEvent {
    /// 変換中の preedit テキスト (画面に下線付きで表示)。`cursor` は preedit 内の選択範囲。
    Preedit { text: String, cursor: Option<(usize, usize)> },
    /// 確定テキスト。focused widget は cursor 位置にこの文字列を挿入する。
    Commit(String),
    /// (M15) OS text store (TSF) 由来の **任意 range 置換**。`[start_byte, end_byte)` を `text` で
    /// 置換し cursor を `new_cursor` へ collapse する。byte offset は直近フレームに widget が
    /// publish した text に対する。
    ///
    /// rtry のまぜ書き変換 / MS-IME の再変換は selection でない range (= 既に確定済みの読み) を
    /// 書き換えるため、`Commit` (selection 置換) では表せない。`Commit(s)` はこの variant の
    /// `ReplaceRange { start_byte: sel_lo, end_byte: sel_hi, text: s, new_cursor: sel_lo + s.len() }`
    /// に相当する特殊形 (widget の `replace_range(min..max, new)` 不変条件を共有する)。
    ReplaceRange {
        start_byte: usize,
        end_byte: usize,
        text: String,
        new_cursor: usize,
    },
    /// (M15) OS text store (TSF) 由来の **selection 変更** (テキストは変えない)。MS-IME 再変換が
    /// 対象範囲を選択する際などに来る。byte offset は直近 publish した text に対する。
    SetSelection { anchor_byte: usize, cursor_byte: usize },
}

/// 1 フレーム分の入力一式。`UiHost::frame` に渡す。
#[derive(Debug, Default)]
pub struct FrameInput {
    pub pointer: PointerFrame,
    pub keyboard: Vec<KeyEvent>,
    pub ime: Vec<ImeEvent>,
    /// M8 Phase 32: このフレームで OS から drop された file 群 (None = drop なし)。
    /// `Ui::take_file_drop_in_rect(rect)` で widget が消費する。
    pub file_drop: Option<DroppedFiles>,
    /// M8 Phase 32: 現在 hover 中の file 一覧 (None = hover なし)。
    /// drop target highlight に使う。consume されない (read-only)。
    pub file_hover: Option<Vec<PathBuf>>,
}

/// 連続フレームをまたいで保持する入力状態。
///
/// 5 つの bool は primary/secondary press/release transition + secondary_pressed の
/// canonical な pointer state で、`PointerFrame` と同じ理由で `struct_excessive_bools` は allow。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct InputAccumulator {
    cur_pos: Option<(f32, f32)>,
    primary_pressed: bool,
    pending_just_pressed: bool,
    pending_just_released: bool,
    pending_secondary_just_pressed: bool,
    pending_secondary_just_released: bool,
    secondary_pressed: bool,
    pending_keys: Vec<KeyEvent>,
    pending_ime: Vec<ImeEvent>,
    /// 直近の `AppEvent::ModifiersChanged` で受け取った修飾キー状態。
    /// フレームをまたいで持続する (next ModifiersChanged まで現状維持)。
    modifiers: Modifiers,
    /// このフレーム内に蓄積されたスクロール量 (物理 px、`take_frame` で reset)。
    accumulated_scroll: (f32, f32),
    /// M8 Phase 32: このフレームに drop された file 群 (`take_input` で `FrameInput::file_drop` に
    /// 移される)。winit は file を 1 つずつ通知するのでここで蓄積。
    pending_file_drops: Vec<PathBuf>,
    /// M8 Phase 32: 現在 hover 中の file 一覧 (フレーム間で持続)。
    /// `FileHovered` で push、`FileHoverCancelled` / `FileDropped` でクリア。
    hovering_files: Vec<PathBuf>,
}

impl InputAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// プラットフォームから受け取ったイベントを取り込む。
    pub fn ingest(&mut self, ev: &AppEvent) {
        match ev {
            AppEvent::PointerMoved(PhysicalPosition { x, y }) => {
                self.cur_pos = Some((*x as f32, *y as f32));
            }
            AppEvent::PointerLeft => {
                self.cur_pos = None;
            }
            AppEvent::PointerInput { button: MouseButton::Left, state } => match state {
                ElementState::Pressed => {
                    if !self.primary_pressed {
                        self.pending_just_pressed = true;
                    }
                    self.primary_pressed = true;
                }
                ElementState::Released => {
                    if self.primary_pressed {
                        self.pending_just_released = true;
                    }
                    self.primary_pressed = false;
                }
            },
            AppEvent::PointerInput { button: MouseButton::Right, state } => match state {
                ElementState::Pressed => {
                    if !self.secondary_pressed {
                        self.pending_secondary_just_pressed = true;
                    }
                    self.secondary_pressed = true;
                }
                ElementState::Released => {
                    if self.secondary_pressed {
                        self.pending_secondary_just_released = true;
                    }
                    self.secondary_pressed = false;
                }
            },
            AppEvent::Scroll(delta) => {
                let (dx, dy) = match delta {
                    ScrollDelta::Lines { x, y } => (x * LINE_HEIGHT_PX, y * LINE_HEIGHT_PX),
                    ScrollDelta::Pixels { x, y } => (*x as f32, *y as f32),
                };
                self.accumulated_scroll.0 += dx;
                self.accumulated_scroll.1 += dy;
            }
            AppEvent::Keyboard(key) => {
                // フレーム間蓄積。order を保つために push 順で並べる。
                self.pending_keys.push(key.clone());
            }
            AppEvent::ImePreedit { text, cursor } => {
                self.pending_ime.push(ImeEvent::Preedit {
                    text: text.clone(),
                    cursor: *cursor,
                });
            }
            AppEvent::ImeCommit(text) => {
                self.pending_ime.push(ImeEvent::Commit(text.clone()));
            }
            AppEvent::ModifiersChanged(m) => {
                self.modifiers = *m;
            }
            AppEvent::FileHovered(path) => {
                self.hovering_files.push(path.clone());
            }
            AppEvent::FileHoverCancelled => {
                self.hovering_files.clear();
            }
            AppEvent::FileDropped(path) => {
                self.pending_file_drops.push(path.clone());
                self.hovering_files.clear();
            }
            AppEvent::Focus(false) => {
                // (review) focus 喪失 (Alt+Tab 等) 時、 winit (Windows) は
                // WM_CAPTURECHANGED で capture を捨てるだけで合成 Release を
                // 送らない。 押下状態を残すと (a) release-event 依存の drag
                // (fader / knob / scrub / scroll 等) が貼り付いたまま、 (b) 復帰後の
                // 最初の press が `!primary_pressed` gate で just_pressed に
                // ならず初回クリックが失われる。 押下中なら合成 release を
                // 積んでから状態を落とす (egui 等と同じ対処)。
                if self.primary_pressed {
                    self.pending_just_released = true;
                }
                self.primary_pressed = false;
                if self.secondary_pressed {
                    self.pending_secondary_just_released = true;
                }
                self.secondary_pressed = false;
            }
            _ => {}
        }
    }

    /// フレーム頭で pointer snapshot を取り、just_pressed/just_released とスクロール累積をリセット。
    pub fn take_frame(&mut self) -> PointerFrame {
        let frame = PointerFrame {
            pos: self.cur_pos,
            primary_just_pressed: self.pending_just_pressed,
            primary_just_released: self.pending_just_released,
            primary_pressed: self.primary_pressed,
            secondary_just_pressed: self.pending_secondary_just_pressed,
            secondary_just_released: self.pending_secondary_just_released,
            modifiers: self.modifiers,
            scroll_delta: self.accumulated_scroll,
        };
        self.pending_just_pressed = false;
        self.pending_just_released = false;
        self.pending_secondary_just_pressed = false;
        self.pending_secondary_just_released = false;
        self.accumulated_scroll = (0.0, 0.0);
        frame
    }

    /// このフレーム分の蓄積されたキー入力イベントを取り出す。
    /// 取り出すと内部のバッファは空になる (フレーム間で持ち越さない)。
    pub fn take_keyboard_events(&mut self) -> Vec<KeyEvent> {
        std::mem::take(&mut self.pending_keys)
    }

    /// このフレーム分の蓄積された IME イベントを取り出す。
    pub fn take_ime_events(&mut self) -> Vec<ImeEvent> {
        std::mem::take(&mut self.pending_ime)
    }

    /// pointer / keyboard / ime / file drop をまとめて取り出す。`UiHost::frame` への引数として渡す。
    pub fn take_input(&mut self) -> FrameInput {
        let pointer = self.take_frame();
        let file_drop = if self.pending_file_drops.is_empty() {
            None
        } else {
            let position = pointer.pos.unwrap_or((0.0, 0.0));
            Some(DroppedFiles {
                paths: std::mem::take(&mut self.pending_file_drops),
                position,
            })
        };
        let file_hover = if self.hovering_files.is_empty() {
            None
        } else {
            Some(self.hovering_files.clone())
        };
        FrameInput {
            pointer,
            keyboard: self.take_keyboard_events(),
            ime: self.take_ime_events(),
            file_drop,
            file_hover,
        }
    }
}
