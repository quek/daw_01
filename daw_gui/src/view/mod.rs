// gui_01 (daw-ui) ベースの view モジュール群。

#[cfg(windows)]
pub mod plugin_embed;

pub mod arrangement_view;
pub mod audio_editor;
pub mod bottom_panel;
pub mod mixer_strips;
pub mod piano_roll_view;
pub mod plugin_picker;
pub mod recovery_modal;
pub mod root;
pub mod runner;
pub mod shortcuts;
pub mod snap;
pub mod status_bar;
pub mod track_inspector;
pub mod transport;
pub mod window;
