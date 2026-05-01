//! ウィジェット実装。
//! M1: label / button。
//! M2: waveform (波形表示、line strip パイプラインを使う)。
//! M3: fader (垂直スライダ) / knob (回転ノブ) / checkbox (bool toggle)。

pub mod button;
pub mod checkbox;
pub mod fader;
pub mod knob;
pub mod label;
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
