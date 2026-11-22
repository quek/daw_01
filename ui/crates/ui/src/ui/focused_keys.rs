//! focus を持つ widget 宛てのキー / IME 入力の取り出し。
//!
//! どれも **フレーム開始時の focus** (`Ui::focused`) で判定する。これによって、
//! 「同フレームに click でフォーカスを取った直後に、その widget が直前まで
//! 流れていたキー入力を遡って消費してしまう」事故を防ぐ
//! (= 例: 何かを打鍵中に間違えて click した場合、打鍵は古い focus に流すのが正解)。
//! 真のモーダル中 (M14 Phase 94 / daw_01 #065) は背景の focused widget を遮断し、**drain しない**
//! ので、modal body の text_input (drawing_in_popup) が同じ入力を取得できる。
//!
//! `ui.rs` からの切り出し (不変条件 9 のサイズ budget)。

use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

use super::Ui;
use crate::id::WidgetId;
use crate::input::ImeEvent;

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// `wid` がフォーカスを持っているならフレームに溜まったキー入力を取り出す。
    /// 取り出すと内部 buffer は空になるので、フレーム内で 1 回だけ呼ぶこと。
    pub fn take_keyboard_events_if_focused(&mut self, wid: WidgetId) -> Vec<KeyEvent> {
        if self.keyboard_blocked_by_modal() || self.focused != Some(wid) {
            return Vec::new();
        }
        std::mem::take(self.keyboard_events)
    }

    /// `wid` 宛てのキー入力のうち、 `keys` の **押下** だけを先に取り出す (順序は保つ)。
    /// 取り出さなかったキーは `wid` の widget にそのまま届く。
    ///
    /// 入力欄に focus を置いたまま、 欄が使わない / 譲ってよい意味をキーに持たせたい caller
    /// (欄の外の候補を巡回する・隣の項目へ移る) が、 欄を描くより **前に** 呼ぶ。 key grab
    /// ([`crate::UiHost::set_key_grab`]) は入力中に押下を横取りしない (文字として欄へ届ける) ので、
    /// 欄の上で使うキーの横取りはこちら。
    pub fn take_focused_key_presses(&mut self, wid: WidgetId, keys: &[PhysicalKey]) -> Vec<KeyEvent> {
        if self.keyboard_blocked_by_modal() || self.focused != Some(wid) || keys.is_empty() {
            return Vec::new();
        }
        let (taken, rest): (Vec<KeyEvent>, Vec<KeyEvent>) = std::mem::take(self.keyboard_events)
            .into_iter()
            .partition(|e| e.state == ElementState::Pressed && keys.contains(&e.physical_key));
        *self.keyboard_events = rest;
        taken
    }

    /// `wid` がフォーカスを持っているならフレームに溜まった IME イベントを取り出す。
    pub fn take_ime_events_if_focused(&mut self, wid: WidgetId) -> Vec<ImeEvent> {
        if self.keyboard_blocked_by_modal() || self.focused != Some(wid) {
            return Vec::new();
        }
        std::mem::take(self.ime_events)
    }
}
