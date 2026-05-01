//! daw-ui-core — Hybrid 即時モード GUI API。
//!
//! 公開する中心型:
//! - `Ui<'a>`: 1フレームの間 `&'a Model` を借りて UI を構築するコンテキスト
//! - `Edit<'a>`: ユーザ操作から発生したエディット (apply はアプリ側責務)
//! - `HeavyCtx`: ピアノロール等の巨大ビュー向け retained-mode 風キャッシュ脱出口
//!
//! 設計上の不変条件:
//! - ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` を要求しない
//! - メッセージ型は導入しない (Edit は enum + `Box<dyn FnOnce>`)
//! - derive マクロは禁止 (Lens 不要、ユーザは手書きクロージャでアクセサを書く)

pub mod edit;
pub mod id;
pub mod input;
pub mod layout;
pub mod ui;
pub mod widgets;

pub use edit::Edit;
pub use id::WidgetId;
pub use input::{FrameInput, ImeEvent, InputAccumulator, PointerFrame};
pub use layout::LayoutPass;
pub use ui::{Ui, UiHost};
pub use widgets::checkbox::CheckboxResponse;
pub use widgets::fader::FaderResponse;
pub use widgets::knob::KnobResponse;
pub use widgets::text_input::TextInputResponse;
pub use widgets::waveform::{
    ChannelLayout, SampleSlices, WaveformHit, WaveformRenderMode, WaveformResponse,
    WaveformSource, WaveformStyle, WaveformView,
};
