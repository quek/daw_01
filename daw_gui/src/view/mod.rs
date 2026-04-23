pub mod arrangement;
#[cfg(windows)]
pub mod plugin_embed;
pub mod plugin_picker;
pub mod status_bar;
pub mod track_inspector;
pub mod transport;

pub use arrangement::ArrangementView;
pub use plugin_picker::PluginPickerView;
pub use status_bar::StatusBarView;
pub use track_inspector::TrackInspectorView;
pub use transport::TransportView;
