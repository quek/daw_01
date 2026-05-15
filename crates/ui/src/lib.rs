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
pub mod snap;
pub mod text_metrics;
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
pub use widgets::drag_in_rect::{DragInfo, DragKind};
pub use widgets::drag_rect::DragRect;
pub use layout::{FlexDirection, Gap, LayoutPass, NodeId, Padding};
pub use scenegraph::{CachedCommands, SceneNode, Scenegraph, hash_inputs};
pub use snap::{SnapConfig, SnapMode};
pub use time::{TimeDisplay, TimeMapping};
pub use daw_ui_platform::CursorIcon;
pub use ui::{FrameStats, Ui, UiHost};
pub use viewport::ViewportState1D;
pub use widgets::level_meter::{LevelMeterStyle, MeterBallistic};
pub use widgets::split_view::Orientation;
pub use widgets::time_grid::{BarBeatGridStyle, TimeRulerStyle};
pub use widgets::arrangement::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementClip, ArrangementClipAudioEdit, ArrangementCurveKind, ArrangementEditRequest,
    ArrangementMasterRow, ArrangementResponse, ArrangementStyle, ArrangementTrack, ArrangementView,
    AutomationClipKey, AutomationLaneHeaderLayout, AutomationLaneKey, AutomationPointKey,
    ClipDragKind, ClipFadeCurveDelta, ClipFadeDelta, ClipGainDelta, ClipKey, FadeCurve, FadeEdge,
    MASTER_TRACK_ID, MoveAutomationClipDelta, MoveAutomationPointDelta, MoveClipDelta,
    ResizeAutomationClipDelta, ResizeClipDelta, SelectModifier, SetAutomationCurveParamKind,
    automation_clip_zone_at, automation_lane_at, automation_lane_header_layout,
    automation_lane_key_at_y, automation_lane_resize_splitter_at, automation_lanes_total_h,
    automation_point_at, clip_hit, clip_to_rect, effective_master_row_h, lane_disclosure_rect_for,
    master_row_lanes_total_h, master_row_total_h, track_index_from_y,
    track_row_height, track_row_resize_splitter_at, visible_track_row_tops,
};
pub use widgets::ruler_ops::{
    LoopBandHit, LoopDragKind, LoopDragSession, PlayheadDragSession,
    compute_loop_drag_endpoints, loop_band_hit_kind,
};
pub use widgets::automation::{AutomationCurveResponse, AutomationCurveStyle};
pub use widgets::checkbox::CheckboxResponse;
pub use widgets::fader::FaderResponse;
pub use widgets::heavy::HeavyCtx;
pub use widgets::list_view::{ListViewResponse, ListViewStyle};
pub use widgets::menu::MenuItemSpec;
pub use widgets::modal::ModalStyle;
pub use widgets::knob::KnobResponse;
pub use widgets::piano_roll::{
    Note, NoteDragKind, NoteFillFn, NoteId, MoveDelta, PianoRollEditRequest, PianoRollResponse,
    PianoRollStyle, PianoRollView, ResizeDelta, VelocityUpdate, default_velocity_color,
    is_black_key, note_hit, note_hover_cursor, note_to_rect, rects_intersect, split_into_morae,
};
pub use widgets::reorderable_list::{
    ReorderableListEditRequest, ReorderableListResponse, ReorderableListStyle,
};
pub use widgets::scrubable_number::{
    ScrubableNumberFormat, ScrubableNumberResponse, ScrubableNumberStyle,
};
pub use widgets::text_input::TextInputResponse;
pub use widgets::toggle_button::{ToggleButtonResponse, ToggleButtonStyle};
pub use widgets::waveform::{
    ChannelLayout, SampleSlices, WaveformHit, WaveformRenderMode, WaveformResponse,
    WaveformSource, WaveformStyle, WaveformView,
};
