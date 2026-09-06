//! 生キー横取り (key grab) — 「押している間だけ」 の意味を持つキー入力を、
//! shortcut 層より **前** で取り出す仕組み (daw_01 r.md #113 の仮想鍵盤が初例)。
//!
//! shortcut は `Pressed` の立ち上がりを 1 コマンドに変換する層で、`Released` を
//! 誰にも渡さない (`Shortcut::matches` は Pressed のみ)。鍵盤のように「離したら止める」
//! が要る用途は、宣言した physical key の **press と release の両方** を生のまま
//! 受け取る必要がある。それをこの層が担い、横取りしたイベントは shortcut 層にも
//! focused widget にも渡らない (= 同じキーに bind された shortcut は宣言中は効かない)。
//!
//! ライブラリはキーの意味 (どの音か) を知らない。宣言する側 (アプリ) が
//! [`GrabbedKey`] を自分の意味に翻訳する。
//!
//! 横取りの規則:
//! - **Pressed** は command 修飾 (Ctrl / Alt / Logo) が無いときだけ横取りする
//!   (Ctrl+Z の undo 等はそのまま shortcut 層へ)。Shift は横取りする (Shift+`[` の
//!   ような「同じキーの別の意味」 を宣言側で使えるように、`shift` を添えて渡す)。
//! - **Released** は修飾や typing / modal の状態に関わらず **常に** 横取りする。
//!   押した後に Ctrl を押した / テキスト欄に focus した / モーダルが開いた、 のどれでも
//!   離す動作を届けないと押しっぱなしの音が残る (stuck note)。
//! - typing 中 (`typing_lock`) と真のモーダル中は Pressed を横取りしない (テキスト欄には
//!   文字として届く / モーダルの背後で鳴らない)。

use std::collections::HashSet;

use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey};

/// 横取りしたキー 1 件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrabbedKey {
    pub key: PhysicalKey,
    /// `true` = 押した、 `false` = 離した。
    pub pressed: bool,
    /// OS の auto-repeat 由来 (押しっぱなし)。鍵盤は無視する。
    pub repeat: bool,
    /// イベント時点で Shift が押されていたか。
    pub shift: bool,
}

/// 宣言中の横取り対象キー集合。`UiHost` がフレームを跨いで保持する。
#[derive(Debug, Default)]
pub struct KeyGrab {
    keys: HashSet<PhysicalKey>,
}

impl KeyGrab {
    /// 横取り対象を `keys` に置き換える (空 = 横取りしない)。
    pub fn set(&mut self, keys: impl IntoIterator<Item = PhysicalKey>) {
        self.keys.clear();
        self.keys.extend(keys);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `events` から横取り対象を **取り除いて** 返す (順序は保つ)。
    /// `block_pressed` = typing 中 / 真のモーダル中 (Pressed を横取りしない)。
    pub fn take_from(
        &self,
        events: &mut Vec<KeyEvent>,
        mods: Modifiers,
        block_pressed: bool,
    ) -> Vec<GrabbedKey> {
        if self.keys.is_empty() {
            return Vec::new();
        }
        let command_mod = mods.ctrl || mods.alt || mods.logo;
        let mut grabbed = Vec::new();
        events.retain(|ev| {
            if !self.keys.contains(&ev.physical_key) {
                return true;
            }
            let pressed = matches!(ev.state, ElementState::Pressed);
            if pressed && (block_pressed || command_mod) {
                return true;
            }
            grabbed.push(GrabbedKey { key: ev.physical_key, pressed, repeat: ev.repeat, shift: mods.shift });
            false
        });
        grabbed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: PhysicalKey, state: ElementState) -> KeyEvent {
        KeyEvent { state, text: None, physical_key: key, repeat: false }
    }

    fn grab_z() -> KeyGrab {
        let mut g = KeyGrab::default();
        g.set([PhysicalKey::Char('Z')]);
        g
    }

    #[test]
    fn 宣言したキーの_press_と_release_だけ横取りして残りは残す() {
        let g = grab_z();
        let mut events = vec![
            ev(PhysicalKey::Char('Z'), ElementState::Pressed),
            ev(PhysicalKey::Char('A'), ElementState::Pressed),
            ev(PhysicalKey::Char('Z'), ElementState::Released),
        ];
        let got = g.take_from(&mut events, Modifiers::default(), false);
        assert_eq!(
            got,
            vec![
                GrabbedKey { key: PhysicalKey::Char('Z'), pressed: true, repeat: false, shift: false },
                GrabbedKey { key: PhysicalKey::Char('Z'), pressed: false, repeat: false, shift: false },
            ]
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].physical_key, PhysicalKey::Char('A'));
    }

    /// Ctrl+Z (undo) は横取りしない。 Shift+Z は横取りして shift を添える。
    #[test]
    fn command_修飾付きの_press_は横取りしない() {
        let g = grab_z();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        let mut events = vec![ev(PhysicalKey::Char('Z'), ElementState::Pressed)];
        assert!(g.take_from(&mut events, ctrl, false).is_empty());
        assert_eq!(events.len(), 1, "Ctrl+Z は shortcut 層へ残す");

        let shift = Modifiers { shift: true, ..Modifiers::default() };
        let mut events = vec![ev(PhysicalKey::Char('Z'), ElementState::Pressed)];
        let got = g.take_from(&mut events, shift, false);
        assert_eq!(got.len(), 1);
        assert!(got[0].shift);
        assert!(events.is_empty());
    }

    /// typing / modal 中: Pressed は横取りしない (文字として届く) が、
    /// Released は常に横取りする (押しっぱなしを残さない)。
    #[test]
    fn typing_中は_press_を残し_release_は常に横取りする() {
        let g = grab_z();
        let mut events = vec![
            ev(PhysicalKey::Char('Z'), ElementState::Pressed),
            ev(PhysicalKey::Char('Z'), ElementState::Released),
        ];
        let got = g.take_from(&mut events, Modifiers::default(), true);
        assert_eq!(got.len(), 1);
        assert!(!got[0].pressed);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].state, ElementState::Pressed));
    }

    /// Ctrl を押しながら離しても release は届く (押した後に修飾を足した場合の stuck 防止)。
    #[test]
    fn 修飾付きでも_release_は横取りする() {
        let g = grab_z();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        let mut events = vec![ev(PhysicalKey::Char('Z'), ElementState::Released)];
        let got = g.take_from(&mut events, ctrl, false);
        assert_eq!(got.len(), 1);
        assert!(events.is_empty());
    }

    #[test]
    fn 宣言が空なら何もしない() {
        let g = KeyGrab::default();
        let mut events = vec![ev(PhysicalKey::Char('Z'), ElementState::Pressed)];
        assert!(g.take_from(&mut events, Modifiers::default(), false).is_empty());
        assert_eq!(events.len(), 1);
    }
}
