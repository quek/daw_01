// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! AppData ステートマシン系 integration test の統合バイナリ。
//!
//! 旧来は dirty_guard / pending_state_queue / group_track_lifecycle /
//! state_roundtrip_watchdog / plugin_load_failure が `tests/*.rs` の独立
//! バイナリ 5 本で、(a) make_plugin_db / build_app / drain 等の fixture が
//! ほぼ verbatim で 5 重複、(b) `cargo test` のたびに daw_gui の依存グラフ
//! 全体 (windows/wgpu/tokio/rsmpeg 等) を 5 回フルリンクしていた。
//! `tests/app_state/main.rs` 配下のサブモジュールに統合してリンク 1 回に
//! まとめ、共有 fixture は `support` に一本化した。
//!
//! この 5 本は合成 fixture のみ (実 DLL ロード / サブプロセス起動 / ネットワーク
//! bind / env 変更なし) なので同一バイナリで安全に並走できる。実プラグインを
//! ロードする pdc_real_vst3 / sidechain_real_vst3 は fault isolation のため
//! 独立バイナリのまま残している。

mod support;

mod clip_rename;
mod dirty_guard;
mod group_track_lifecycle;
mod linked_clip_bounds;
mod make_unique;
mod open_stays_clean;
mod pending_state_queue;
mod plugin_load_failure;
mod shutdown_sequence;
mod state_roundtrip_watchdog;
mod sync_flush;
mod track_delete;
mod transform_edit_regress;
