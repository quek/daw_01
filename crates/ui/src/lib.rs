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

pub mod clipboard;
pub mod dialog;
pub mod edit;
pub mod history;
pub mod id;
pub mod input;
pub mod layout;
pub mod popup;
pub mod scenegraph;
pub mod shortcut;
pub mod time;
pub mod ui;
pub mod viewport;
pub mod widgets;

#[cfg(feature = "clipboard")]
pub use clipboard::ArboardClipboard;
pub use clipboard::{ClipboardProvider, NoopClipboard};
pub use dialog::{DialogResult, FileDialogFilter};
pub use edit::Edit;
pub use history::{HistoryEntry, HistoryStack};
pub use id::WidgetId;
pub use shortcut::{Shortcut, ShortcutMap, ShortcutParseError};
pub use input::{DroppedFiles, FrameInput, ImeEvent, InputAccumulator, PointerFrame};
pub use widgets::drag_rect::DragRect;
pub use layout::{FlexDirection, Gap, LayoutPass, NodeId, Padding};
pub use scenegraph::{CachedCommands, SceneNode, Scenegraph, hash_inputs};
pub use time::{TimeDisplay, TimeMapping};
pub use daw_ui_platform::CursorIcon;
pub use ui::{Ui, UiHost};
pub use viewport::ViewportState1D;
pub use widgets::level_meter::{LevelMeterStyle, MeterBallistic};
pub use widgets::split_view::Orientation;
pub use widgets::time_grid::{BarBeatGridStyle, TimeRulerStyle};
pub use widgets::automation::{AutomationCurveResponse, AutomationCurveStyle};
pub use widgets::checkbox::CheckboxResponse;
pub use widgets::fader::FaderResponse;
pub use widgets::heavy::HeavyCtx;
pub use widgets::knob::KnobResponse;
pub use widgets::text_input::TextInputResponse;
pub use widgets::waveform::{
    ChannelLayout, SampleSlices, WaveformHit, WaveformRenderMode, WaveformResponse,
    WaveformSource, WaveformStyle, WaveformView,
};
