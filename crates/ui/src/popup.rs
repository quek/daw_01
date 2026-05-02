//! `popup_layer` — modal な popup / menu / dropdown / context_menu の共通基盤 (M7 Phase 25)。
//!
//! 設計:
//! - popup の open / close 状態は `UiHost` に `HashMap<WidgetId, PopupOpenState>` で保持
//! - `Ui::popup_layer(id, |ui| ...)` で「open ならば描画」、closure 内の primitive は
//!   **deferred buffer** に積まれ、frame 末尾で base scene に append (z-order = 最前面)
//! - `Ui::open_popup(id, anchor, modal)` で popup を開く (例: `menu_bar` が File menu の click で呼ぶ)
//! - `Ui::close_popup(id)` で popup を閉じる
//! - 外クリック検出は popup_layer 内で実装、自動 close
//! - modal: M7 では popup の anchor 外クリックを popup_layer 自身が消費する形 (他 widget は
//!   `pointer.primary_just_*` がそのまま見えるので、利用者が popup_layer を user closure の
//!   早い段階に置くこと。この前提は `feedback_pursue_best_practice` の妥協ポイントとして
//!   `docs/history.md` に記録)

use daw_ui_renderer::Rect;

use crate::id::WidgetId;

/// popup が現在 open している間 `UiHost` に保持される情報。
#[derive(Debug, Clone, Copy)]
pub struct PopupOpenState {
    /// popup を開く起点となった矩形 (例: menu_bar の "File" ボタン)。
    /// 外クリック判定で「この anchor 外のクリック = close」に使う場合がある (利用者次第)。
    pub anchor: Rect,
    /// modal なら他 widget の click を抑制する。M7 では popup 自身が責任。
    pub modal: bool,
    /// popup を開く前に focus を持っていた widget。close で戻す。
    pub prev_focus: Option<WidgetId>,
}
