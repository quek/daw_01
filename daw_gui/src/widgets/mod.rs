//! daw_gui 固有の library widget。
//!
//! daw-ui-core は Model 非依存の汎用 immediate-mode プリミティブ (button / fader /
//! waveform / heavy キャッシュ …) を提供する。 arrangement のように **`common::model`
//! (Song / Track / Clip) と `AppData` を直接読み、 `edit_song` 経由で `Edit<AppData>` を
//! 直発行する** DAW 固有の複合 widget は daw-ui-core には置けない (Model 非依存の不変条件を
//! 破る)。 それらをここに置く (S4b で `ui/crates` から移設)。
//!
//! 汎用プリミティブ (heavy / push_rect / take_drag_rect_in_rect …) は引き続き
//! daw-ui-core の `pub` API を呼ぶ。

pub mod arrangement;
pub mod piano_roll;
pub mod ruler_ops;
pub mod time_grid;
