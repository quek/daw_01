//! 入力状態 — `AppEvent` を蓄積してフレーム毎に Ui に渡す形にする。

use daw_ui_platform::{AppEvent, ElementState, MouseButton, PhysicalPosition};

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
}

/// 連続フレームをまたいで保持する入力状態。
#[derive(Debug, Default)]
pub struct InputAccumulator {
    cur_pos: Option<(f32, f32)>,
    primary_pressed: bool,
    pending_just_pressed: bool,
    pending_just_released: bool,
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
            _ => {}
        }
    }

    /// フレーム頭で snapshot を取り、just_pressed/just_released をリセット。
    pub fn take_frame(&mut self) -> PointerFrame {
        let frame = PointerFrame {
            pos: self.cur_pos,
            primary_just_pressed: self.pending_just_pressed,
            primary_just_released: self.pending_just_released,
            primary_pressed: self.primary_pressed,
        };
        self.pending_just_pressed = false;
        self.pending_just_released = false;
        frame
    }
}
