//! r.md #158: トラックインスペクタの見せ方のイベント (`docs/plan_rmd_158_inspector.md` §2.5 / §2.6)。
//!
//! [`AppEvent::Inspector`](crate::event::AppEvent::Inspector) が包む 1 本に集約する
//! (`VirtualKeyboardEvent` と同じ「1 arm = 1 サブ enum」)。処理は
//! [`AppData::handle_inspector_event`](crate::state::AppData::handle_inspector_event)。
//! どれも「見方の都合」で、Song を書かない (undo / dirty の対象外)。

use crate::state::inspector_view::{InspectorBand, InspectorDetail};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InspectorEvent {
    /// 帯の見出しのクリック (帯ごとに独立して開閉、種類ごとに共通)。
    ToggleBand(InspectorBand),
    /// 帯の中の「詳細」の行のクリック。
    ToggleDetail(InspectorDetail),
    /// インスペクタ全体を出す / 隠す (`I` / View メニュー)。app_config に保存する。
    ToggleVisible,
    /// 右端のドラッグで幅を変える。ドラッグ中は値だけ、`commit` (離した) で app_config へ書く。
    SetWidth { w: f32, commit: bool },
}
