pub mod arrangement_view;
pub mod bottom_panel;
pub mod lyric_panel;
pub mod mixer_strips;
pub mod piano_roll_view;
#[cfg(windows)]
pub mod plugin_embed;
pub mod plugin_picker;
pub mod status_bar;
pub mod track_inspector;
pub mod transport;

pub use arrangement_view::ArrangementView;
pub use bottom_panel::BottomPanelView;
pub use plugin_picker::PluginPickerView;
pub use status_bar::StatusBarView;
pub use track_inspector::TrackInspectorView;
pub use transport::TransportView;
