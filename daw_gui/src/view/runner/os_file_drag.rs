//! r.md #141: アレンジ widget が予約した OS のファイルドラッグ (`ui_ephemeral.pending_os_file_drag`)
//! をフレームの後に行い、その後始末をする。`Runner` の private field を触るので子モジュール。
//!
//! - `DoDragDrop` のループがマウスの WM_LBUTTONUP を食うので、winit からは release が届かない
//!   → 戻った後に合成の release を流す (流さないと daw-ui はボタンを押したままだと思い続ける)。
//! - ドラッグを daw_01 自身の窓へ戻して離すと、winit の drop target が受け取って
//!   `HoveredFile` / `DroppedFile` を (ループ中は buffer して) 後から流す。自分が出したファイルの
//!   それは捨てる (= 「窓に戻して離しても何も起きない」)。捨てる対象は次の本物のマウスボタン入力まで
//!   — buffer された分は必ずそれより先に届く。

use super::*;

impl Runner {
    /// `RedrawRequested` の `render_frame` の後に呼ぶ (`state` の借用を手放した位置)。
    pub(super) fn run_pending_os_file_drag(&mut self) {
        let Some(state) = self.state.as_mut() else { return };
        if state.app.ui_ephemeral.pending_os_file_drag.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut state.app.ui_ephemeral.pending_os_file_drag);
        #[cfg(windows)]
        let hwnd = state.app.ui_ephemeral.main_window_hwnd;
        #[cfg(not(windows))]
        let hwnd: Option<isize> = None;
        let Some(hwnd) = hwnd else { return };
        tracing::info!(files = ?paths, "OS file drag start");
        match crate::os_file_drag::run(hwnd, &paths) {
            Ok(accepted) => tracing::info!(accepted, "OS file drag end"),
            Err(e) => tracing::warn!(error = %e, "OS file drag failed"),
        }
        self.own_dragged_files = paths;
        self.sync_pointer_from_os();
        self.dispatch_platform_event(PlatformEvent::PointerInput {
            button: map_button(winit::event::MouseButton::Left),
            state: map_state(winit::event::ElementState::Released),
        });
    }

    /// 自分が OS ドラッグで出したファイルが自分の窓に戻ってきた file イベントか。
    pub(super) fn is_own_dragged_file_event(&self, event: &WindowEvent) -> bool {
        if self.own_dragged_files.is_empty() {
            return false;
        }
        match event {
            WindowEvent::HoveredFile(p) | WindowEvent::DroppedFile(p) => self.own_dragged_files.contains(p),
            WindowEvent::HoveredFileCancelled => true,
            _ => false,
        }
    }
}
