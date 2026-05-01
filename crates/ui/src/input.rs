//! 入力状態 — `AppEvent` を蓄積してフレーム毎に Ui に渡す形にする。

use daw_ui_platform::{AppEvent, ElementState, KeyEvent, Modifiers, MouseButton, PhysicalPosition};

/// 1 フレーム分のポインタ入力スナップショット。
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerFrame {
    pub pos: Option<(f32, f32)>,
    /// このフレーム内で「押下された」(pressed transition があった)。
    pub primary_just_pressed: bool,
    /// このフレーム内で「離された」(released transition があった)。
    pub primary_just_released: bool,
    /// 現在押下中。
    pub primary_pressed: bool,
    /// 現在の修飾キー状態 (Ctrl / Shift / Alt / Logo)。Ctrl+drag による高精度モードなどに使う。
    pub modifiers: Modifiers,
}

/// IME (input method editor) のイベント。focused widget が処理する。
#[derive(Debug, Clone)]
pub enum ImeEvent {
    /// 変換中の preedit テキスト (画面に下線付きで表示)。`cursor` は preedit 内の選択範囲。
    Preedit { text: String, cursor: Option<(usize, usize)> },
    /// 確定テキスト。focused widget は cursor 位置にこの文字列を挿入する。
    Commit(String),
}

/// 1 フレーム分の入力一式。`UiHost::frame` に渡す。
#[derive(Debug, Default)]
pub struct FrameInput {
    pub pointer: PointerFrame,
    pub keyboard: Vec<KeyEvent>,
    pub ime: Vec<ImeEvent>,
}

/// 連続フレームをまたいで保持する入力状態。
#[derive(Debug, Default)]
pub struct InputAccumulator {
    cur_pos: Option<(f32, f32)>,
    primary_pressed: bool,
    pending_just_pressed: bool,
    pending_just_released: bool,
    pending_keys: Vec<KeyEvent>,
    pending_ime: Vec<ImeEvent>,
    /// 直近の `AppEvent::ModifiersChanged` で受け取った修飾キー状態。
    /// フレームをまたいで持続する (next ModifiersChanged まで現状維持)。
    modifiers: Modifiers,
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
            _ => {}
        }
    }

    /// フレーム頭で pointer snapshot を取り、just_pressed/just_released をリセット。
    pub fn take_frame(&mut self) -> PointerFrame {
        let frame = PointerFrame {
            pos: self.cur_pos,
            primary_just_pressed: self.pending_just_pressed,
            primary_just_released: self.pending_just_released,
            primary_pressed: self.primary_pressed,
            modifiers: self.modifiers,
        };
        self.pending_just_pressed = false;
        self.pending_just_released = false;
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

    /// pointer / keyboard / ime をまとめて取り出す。`UiHost::frame` への引数として渡す。
    pub fn take_input(&mut self) -> FrameInput {
        FrameInput {
            pointer: self.take_frame(),
            keyboard: self.take_keyboard_events(),
            ime: self.take_ime_events(),
        }
    }
}
