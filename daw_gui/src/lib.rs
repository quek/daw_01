//! daw_gui ライブラリ (binary `daw_gui` と integration test 共通)。
//!
//! `cargo test --test <name>` で `tests/` 配下の integration test が
//! `use daw_gui::...` で `AppData` / `dispatcher` 等の API を参照できる
//! ようにするため、 binary `main.rs` から module 群を切り出して library
//! として公開する。
//!
//! production binary (`src/main.rs`) は `daw_gui::run` を呼ぶ薄い entry
//! 役、 integration test は `AppData::new` を直接呼んで一連の操作を
//! シミュレートする。 dispatcher trait のおかげで winit EventLoop 無しで
//! 構築できる。

pub mod app;
pub mod dispatcher;
pub mod job;
pub mod midi;
pub mod subprocess;
pub mod view;
