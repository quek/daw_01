//! r.md #113: 仮想鍵盤ウィンドウの GUI イベント (`docs/plan_virtual_keyboard.md`)。
//!
//! [`AppEvent::VirtualKeyboard`](crate::event::AppEvent::VirtualKeyboard) が包む 1 本に
//! 集約する (`SamplerEvent` / `LauncherEvent` と同じ「1 arm = 1 サブ enum」)。
//! 処理は [`AppData::handle_virtual_keyboard_event`](crate::state::AppData::handle_virtual_keyboard_event)。

use daw_ui_core::GrabbedKey;
use daw_ui_renderer::Rect;

#[derive(Debug, Clone, PartialEq)]
pub enum VirtualKeyboardEvent {
    /// 窓の開閉 (`K` / View メニュー / ✕ / Esc)。 閉じるときは鳴っている音を全部止める。
    Toggle,
    /// key grab が横取りした PC キー (音 / オクターブ / ベロシティの意味付けは handler)。
    Key(GrabbedKey),
    /// オクターブを `delta` だけ上下 (窓の − / + ボタン、 `[` / `]`)。
    ShiftOctave(i8),
    /// ベロシティを直接指定 (`1..=127` に畳む)。 `commit` で app_config へ保存
    /// (数値欄のドラッグ中は毎フレーム来るので、 立ち下がりだけ保存する)。
    SetVelocity { velocity: u8, commit: bool },
    /// マウスで押している鍵 (`None` = 離した)。 前回値との差分で note-on / off を出す。
    MousePitch(Option<u8>),
    /// 押している音を全部止める (窓の非アクティブ化 / 閉じる)。
    ReleaseAll,
    /// 窓の位置 (タイトルバードラッグの release で確定)。
    SetRect(Rect),
}
