//! ウィジェット実装。
//! M1: label / button。
//! M2: waveform (波形表示、line strip パイプラインを使う)。
//! M3: fader / knob / checkbox / text_input。

pub mod automation;
pub mod button;
pub mod checkbox;
pub mod drag_rect;
pub mod dropdown;
pub mod fader;
pub mod heavy;
pub mod knob;
pub mod label;
pub mod level_meter;
pub mod menu;
pub mod panel;
pub mod piano_roll;
pub mod scroll_area;
pub mod split_view;
pub mod tab_view;
pub mod text_input;
pub mod time_grid;
pub mod toggle_button;
pub mod waveform;

use std::any::Any;

/// ウィジェット永続状態の共通インタフェース (`Box<dyn WidgetState>` で保持するため)。
pub trait WidgetState: Any + Send + Sync {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Send + Sync> WidgetState for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// button / label はそれぞれ `Ui` への inherent impl を提供する形なので、ここでは re-export 不要。
