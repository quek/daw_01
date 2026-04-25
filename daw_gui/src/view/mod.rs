pub mod arrangement;
pub mod mixer_strips;
#[cfg(windows)]
pub mod plugin_embed;
pub mod plugin_picker;
pub mod status_bar;
pub mod track_inspector;
pub mod tracker_mixer;
pub mod transport;

pub use plugin_picker::PluginPickerView;
pub use status_bar::StatusBarView;
pub use track_inspector::TrackInspectorView;
pub use tracker_mixer::TrackerMixerView;
pub use transport::TransportView;
