//! プロジェクトタブの操作 (`docs/plan_project_tabs.md` §5.2)。
//!
//! [`AppEvent::Tab`](crate::event::AppEvent::Tab) が包む [`TabEvent`] 1 本に集約する
//! (`LauncherEvent` / `SamplerEvent` と同じ「1 arm = 1 サブ enum」)。
//! 処理は [`AppData::handle_tab_event`](crate::state::AppData::handle_tab_event)。

use std::path::PathBuf;

use common::protocol::ProjectKey;

#[derive(Debug, Clone, PartialEq)]
pub enum TabEvent {
    /// 新しいタブに空の Untitled プロジェクトを開いてアクティブにする (Ctrl+N / File > New)。
    New,
    /// `path` のプロジェクトを開く。アクティブなタブが pristine (未保存・未編集の
    /// Untitled) ならそのタブを置き換え、そうでなければ新しいタブに開く (Q4)。
    /// 既に別のタブで開いているファイルならそのタブへ切り替える。
    Open(PathBuf),
    /// `key` のタブをアクティブにする (タブ帯のクリック)。
    Switch(ProjectKey),
    /// 右隣のタブへ (端で巡回、Ctrl+Tab)。
    Next,
    /// 左隣のタブへ (端で巡回、Ctrl+Shift+Tab)。
    Prev,
    /// `key` のタブを閉じる (✕ / Ctrl+W)。未保存なら確認、最後の 1 つなら空の Untitled が残る (Q5)。
    Close(ProjectKey),
    /// `key` 以外を全部閉じる (右クリックメニュー)。未保存タブは順に確認、キャンセルで中断。
    CloseOthers(ProjectKey),
    /// 全部閉じる (右クリックメニュー)。最後に空の Untitled が残る。
    CloseAll,
    /// タブ帯のドラッグ並べ替え: `key` を `to` 番目へ。
    Move { key: ProjectKey, to: usize },
}
