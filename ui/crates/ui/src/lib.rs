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
pub mod color;
pub mod dialog;
pub mod edit;
pub mod id;
pub mod input;
pub mod layout;
pub mod popup;
pub mod scenegraph;
pub mod shortcut;
pub mod text_metrics;
pub mod theme;
pub mod ui;
pub mod viewport;
pub mod widgets;

#[cfg(feature = "clipboard")]
pub use clipboard::ArboardClipboard;
pub use clipboard::{ClipboardProvider, NoopClipboard};
pub use dialog::{DialogResult, FileDialogFilter};
pub use edit::Edit;
pub use id::WidgetId;
pub use widgets::WidgetState;
pub use shortcut::{Shortcut, ShortcutMap, ShortcutParseError};
pub use input::{DroppedFiles, FrameInput, ImeEvent, InputAccumulator, PointerFrame};
pub use widgets::drag_in_rect::{DragInfo, DragKind};
pub use widgets::drag_rect::DragRect;
pub use layout::{FlexDirection, Gap, LayoutPass, NodeId, Padding};
pub use scenegraph::{CachedCommands, SceneNode, Scenegraph, hash_inputs};
pub use daw_ui_platform::CursorIcon;
pub use daw_ui_renderer::{available_font_families, Color, TextureHandle, TexturedQuad};
pub use theme::{Palette, WaveformInk, contrast_ratio};
pub use ui::{FrameStats, Ui, UiHost};
pub use viewport::ViewportState1D;
pub use widgets::goniometer::{CorrelationStyle, GoniometerStyle};
pub use widgets::level_meter::{LevelMeterStyle, MeterBallistic, MeterScale};
pub use widgets::loudness_graph::LoudnessGraphStyle;
pub use widgets::loudness_meter::LoudnessMeterStyle;
pub use widgets::oscilloscope::{OscilloscopeStyle, ScopeColumn};
pub use widgets::spectrum::SpectrumStyle;
pub use widgets::split_view::Orientation;
pub use widgets::automation::{AutomationCurveResponse, AutomationCurveStyle};
pub use widgets::modulator_editor::{MsegAction, MsegEditorResponse, MsegEditorStyle, MsegNode};
pub use widgets::channel_fader_meter::ChannelFaderMeterResponse;
pub use widgets::checkbox::CheckboxResponse;
pub use widgets::color_picker::{ColorPickerResponse, ColorPickerStyle};
pub use widgets::fader::FaderResponse;
pub use widgets::heavy::HeavyCtx;
pub use widgets::list_view::{ListViewResponse, ListViewStyle};
pub use widgets::menu::MenuItemSpec;
pub use widgets::modal::ModalStyle;
pub use widgets::knob::{KnobResponse, KnobStyle};
pub use widgets::reorderable_list::{
    ReorderableListEditRequest, ReorderableListResponse, ReorderableListStyle,
};
pub use widgets::scrubable_number::{
    ModEdit, ModEntry, Modulation, ScrubableNumberFormat, ScrubableNumberResponse,
    ScrubableNumberStyle,
};
pub use widgets::button::ButtonTextAlign;
pub use widgets::text_input::{TextInputResponse, TextInputStyle};
pub use widgets::toggle_button::{
    IndicatorButtonResponse, ToggleButtonResponse, ToggleButtonStyle,
};
pub use widgets::waveform::{
    ChannelLayout, SampleSlices, WaveformHit, WaveformRenderMode, WaveformResponse,
    WaveformSegment, WaveformSource, WaveformStyle, WaveformView,
};
