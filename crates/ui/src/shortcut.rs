//! M8 Phase 30: keyboard shortcut。
//!
//! `Shortcut::parse("Ctrl+Shift+Z")` で文字列から構築できる。`ShortcutMap` に
//! `name -> Shortcut` を登録し、フレーム頭で keyboard_events と照合してマッチした
//! `name` を `Ui::pending_shortcuts` に積む。widget は `Ui::take_shortcut(name)` で
//! 1 度だけ消費する (pull 型)。
//!
//! global / context-sensitive の両立: 修飾キーなしの shortcut (Space 等) は
//! `Ui::set_typing_focus(true)` (text_input が focus 中に呼ぶ) のフレームで
//! 抑制される (= keyboard_events に戻して text_input に届く)。

use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey};

/// `Shortcut::try_parse` の失敗内容。`spec` 全体と `reason` (短い説明) を保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutParseError {
    pub spec: String,
    pub reason: String,
}

impl std::fmt::Display for ShortcutParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Shortcut::parse: {} in {:?}", self.reason, self.spec)
    }
}

impl std::error::Error for ShortcutParseError {}

/// shortcut spec (key + 修飾)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub key: PhysicalKey,
    pub mods: Modifiers,
}

impl Shortcut {
    #[must_use]
    pub fn new(key: PhysicalKey, mods: Modifiers) -> Self {
        Self { key, mods }
    }

    /// `"Ctrl+Shift+Z"` のような文字列を Shortcut に解釈する。
    /// 解釈不能な spec は `panic!` する (const literal / `with_default_bindings` のような
    /// 起動時 hard-coded spec 想定 = 起動時に検出されるべき)。
    ///
    /// runtime 由来の spec (ユーザ preference / config file / プラグイン入力) を
    /// 解釈する場合は `try_parse` を使うこと (panic を避けて Result で返す)。
    ///
    /// 受理する modifier トークン (大小無視):
    /// `Ctrl`, `Control`, `Shift`, `Alt`, `Super`, `Cmd`, `Logo`, `Win`
    ///
    /// 受理する key トークン:
    /// - 1 文字 alphabet: `Z` / `z` (大文字に正規化)
    /// - 1 桁数字: `0`..=`9`
    /// - 印字可能記号 (M9 P0-1): `/`, `;`, `,`, `.`, `-`, `=`, `[`, `]`, `\`, `'`, `` ` ``
    ///   (`+` は delimiter のため受理しない)
    /// - 特殊: `Esc` / `Escape`, `Enter` / `Return`, `Space`, `Tab`, `Backspace` / `BS`,
    ///   `Delete` / `Del`, `Home`, `End`, `PageUp`, `PageDown`, `Insert` / `Ins`,
    ///   `Up` / `ArrowUp`, `Down` / `ArrowDown`, `Left` / `ArrowLeft`, `Right` / `ArrowRight`
    /// - ファンクション: `F1` ..= `F24`
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        Self::try_parse(spec).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `parse` の Result 版。runtime 由来の spec を panic させずに解釈する。
    ///
    /// `Err(ShortcutParseError)` のケース:
    /// - `+` 区切りで空トークン (`"Ctrl++Z"`, `"+Z"` 等)
    /// - key トークンを 2 つ以上含む (`"A+B"` 等、modifier は除く)
    /// - key トークンが 1 つもない (`"Ctrl+Shift"` 等)
    /// - 受理されない key トークン (`"Ctrl+Foo"` 等)
    ///
    /// # Errors
    /// 上記のケースで `ShortcutParseError` を返す。
    pub fn try_parse(spec: &str) -> Result<Self, ShortcutParseError> {
        let mut mods = Modifiers::empty();
        let mut key: Option<PhysicalKey> = None;
        for part in spec.split('+') {
            let p = part.trim();
            if p.is_empty() {
                return Err(ShortcutParseError {
                    spec: spec.to_string(),
                    reason: "empty token".to_string(),
                });
            }
            let lower = p.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" => mods.alt = true,
                "super" | "cmd" | "logo" | "win" => mods.logo = true,
                _ => {
                    if key.is_some() {
                        return Err(ShortcutParseError {
                            spec: spec.to_string(),
                            reason: "multiple keys".to_string(),
                        });
                    }
                    key = Some(parse_key_token(p).map_err(|reason| ShortcutParseError {
                        spec: spec.to_string(),
                        reason,
                    })?);
                }
            }
        }
        let key = key.ok_or_else(|| ShortcutParseError {
            spec: spec.to_string(),
            reason: "no key".to_string(),
        })?;
        Ok(Self { key, mods })
    }

    /// `KeyEvent` (Pressed) と現在の修飾キー状態に対してマッチするか。
    #[must_use]
    pub fn matches(&self, ev: &KeyEvent, current_mods: Modifiers) -> bool {
        if !matches!(ev.state, ElementState::Pressed) {
            return false;
        }
        if !current_mods.matches(self.mods) {
            return false;
        }
        ev.physical_key == self.key
    }
}

fn parse_key_token(p: &str) -> Result<PhysicalKey, String> {
    let lower = p.to_ascii_lowercase();
    // 特殊キー (長いものから検査)
    match lower.as_str() {
        "esc" | "escape" => return Ok(PhysicalKey::Escape),
        "enter" | "return" => return Ok(PhysicalKey::Enter),
        "space" => return Ok(PhysicalKey::Space),
        "tab" => return Ok(PhysicalKey::Tab),
        "bs" | "backspace" => return Ok(PhysicalKey::Backspace),
        "del" | "delete" => return Ok(PhysicalKey::Delete),
        "home" => return Ok(PhysicalKey::Home),
        "end" => return Ok(PhysicalKey::End),
        "pageup" | "pgup" => return Ok(PhysicalKey::PageUp),
        "pagedown" | "pgdn" => return Ok(PhysicalKey::PageDown),
        "ins" | "insert" => return Ok(PhysicalKey::Insert),
        "up" | "arrowup" => return Ok(PhysicalKey::ArrowUp),
        "down" | "arrowdown" => return Ok(PhysicalKey::ArrowDown),
        "left" | "arrowleft" => return Ok(PhysicalKey::ArrowLeft),
        "right" | "arrowright" => return Ok(PhysicalKey::ArrowRight),
        _ => {}
    }
    // F1..=F24
    if let Some(n_str) = lower.strip_prefix('f')
        && let Ok(n) = n_str.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Ok(PhysicalKey::F(n));
    }
    // 1 文字 alphabet → Char(uppercased)
    if p.chars().count() == 1 {
        let c = p.chars().next().expect("len 1");
        if c.is_ascii_alphabetic() {
            return Ok(PhysicalKey::Char(c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            // '0' = 0x30
            return Ok(PhysicalKey::Digit(c as u8 - b'0'));
        }
        // M9 P0-1: 印字可能記号 11 種 (`+` は delimiter のため除く)
        if matches!(c, '/' | ';' | ',' | '.' | '-' | '=' | '[' | ']' | '\\' | '\'' | '`') {
            return Ok(PhysicalKey::Char(c));
        }
    }
    Err(format!("unknown key token {p:?}"))
}

/// `name -> Shortcut` の登録テーブル。
///
/// 登録順 (= `bind` の順) を保持し、同じ Shortcut が複数登録された場合は **先勝ち**
/// (= 先に `bind` した name が `matches` で返る)。
#[derive(Debug, Default, Clone)]
pub struct ShortcutMap {
    entries: Vec<(Shortcut, &'static str)>,
}

impl ShortcutMap {
    #[must_use]
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// DAW で慣用される shortcut を一括登録した map。
    ///
    /// 登録される name (シンボリック識別子):
    /// - `"undo"` = Ctrl+Z
    /// - `"redo"` = Ctrl+Shift+Z (+ Ctrl+Y も登録)
    /// - `"cut"` = Ctrl+X
    /// - `"copy"` = Ctrl+C
    /// - `"paste"` = Ctrl+V
    /// - `"select_all"` = Ctrl+A
    /// - `"save"` = Ctrl+S
    /// - `"save_as"` = Ctrl+Shift+S
    /// - `"open"` = Ctrl+O
    /// - `"new"` = Ctrl+N
    /// - `"delete"` = Delete
    /// - `"escape"` = Escape
    /// - `"tab_next"` = Tab
    /// - `"tab_prev"` = Shift+Tab
    ///
    /// **note**: 修飾キーなしの矢印 (`Up` / `Down` / `Left` / `Right`) は **default binding せず**。
    /// shortcut layer は frame 頭で keyboard_events から consume するため、bind すると text_input
    /// 等の内部矢印キー処理 (cursor 移動) を奪ってしまう。focus traversal を入れるときは
    /// `typing_focus` を見て consume を抑制する path を整備する必要がある (M9 Phase 45e bug fix)。
    #[must_use]
    pub fn with_default_bindings() -> Self {
        let mut m = Self::new();
        m.bind("undo", "Ctrl+Z");
        m.bind("redo", "Ctrl+Shift+Z");
        m.bind("redo", "Ctrl+Y");
        m.bind("cut", "Ctrl+X");
        m.bind("copy", "Ctrl+C");
        m.bind("paste", "Ctrl+V");
        m.bind("select_all", "Ctrl+A");
        m.bind("save", "Ctrl+S");
        m.bind("save_as", "Ctrl+Shift+S");
        m.bind("open", "Ctrl+O");
        m.bind("new", "Ctrl+N");
        m.bind("delete", "Delete");
        m.bind("escape", "Escape");
        m.bind("tab_next", "Tab");
        m.bind("tab_prev", "Shift+Tab");
        // M9 Phase 43: debug overlay toggle
        m.bind("debug_overlay_toggle", "Ctrl+F1");
        m
    }

    /// `name` に `spec` で記述された shortcut を割り当てる。同 name が既登録でも追加 (multi-bind)。
    pub fn bind(&mut self, name: &'static str, spec: &str) {
        self.entries.push((Shortcut::parse(spec), name));
    }

    /// `name` の登録を全て削除。
    pub fn unbind(&mut self, name: &'static str) {
        self.entries.retain(|(_, n)| *n != name);
    }

    /// `name` の登録を unbind してから新しい `spec` で bind し直す (preference 反映用)。
    pub fn rebind(&mut self, name: &'static str, spec: &str) {
        self.unbind(name);
        self.bind(name, spec);
    }

    /// `KeyEvent` + 現在 modifier に対して登録済 shortcut のうち先頭の name を返す。
    #[must_use]
    pub fn matches(&self, ev: &KeyEvent, current_mods: Modifiers) -> Option<&'static str> {
        for (sc, name) in &self.entries {
            if sc.matches(ev, current_mods) {
                return Some(*name);
            }
        }
        None
    }

    /// 登録一覧 (UI 表示や preference エクスポート用)。
    pub fn iter(&self) -> impl Iterator<Item = (&Shortcut, &&'static str)> + '_ {
        self.entries.iter().map(|(s, n)| (s, n))
    }

    /// `name` で登録された最初の shortcut の表記文字列 (menu 右端の "Ctrl+Z" 表示用)。
    #[must_use]
    pub fn display_for(&self, name: &'static str) -> Option<String> {
        let (sc, _) = self.entries.iter().find(|(_, n)| *n == name)?;
        Some(format_shortcut(*sc))
    }
}

/// `Shortcut` を表記文字列に戻す ("Ctrl+Shift+Z" 形式)。
fn format_shortcut(sc: Shortcut) -> String {
    let mut s = String::new();
    if sc.mods.ctrl {
        s.push_str("Ctrl+");
    }
    if sc.mods.shift {
        s.push_str("Shift+");
    }
    if sc.mods.alt {
        s.push_str("Alt+");
    }
    if sc.mods.logo {
        s.push_str("Cmd+");
    }
    s.push_str(&format_key(sc.key));
    s
}

fn format_key(k: PhysicalKey) -> String {
    match k {
        PhysicalKey::Escape => "Esc".into(),
        PhysicalKey::Enter => "Enter".into(),
        PhysicalKey::Space => "Space".into(),
        PhysicalKey::Tab => "Tab".into(),
        PhysicalKey::Backspace => "Backspace".into(),
        PhysicalKey::Delete => "Delete".into(),
        PhysicalKey::Home => "Home".into(),
        PhysicalKey::End => "End".into(),
        PhysicalKey::PageUp => "PageUp".into(),
        PhysicalKey::PageDown => "PageDown".into(),
        PhysicalKey::Insert => "Insert".into(),
        PhysicalKey::ArrowUp => "Up".into(),
        PhysicalKey::ArrowDown => "Down".into(),
        PhysicalKey::ArrowLeft => "Left".into(),
        PhysicalKey::ArrowRight => "Right".into(),
        PhysicalKey::Char(c) => c.to_string(),
        PhysicalKey::Digit(n) => char::from(b'0' + n).to_string(),
        PhysicalKey::F(n) => format!("F{n}"),
        PhysicalKey::Other(n) => format!("Other({n})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daw_ui_platform::ElementState;

    fn key(physical: PhysicalKey) -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: physical,
        }
    }

    #[test]
    fn parse_simple_alphabet() {
        let sc = Shortcut::parse("Ctrl+Z");
        assert_eq!(sc.key, PhysicalKey::Char('Z'));
        assert!(sc.mods.ctrl && !sc.mods.shift && !sc.mods.alt && !sc.mods.logo);
    }

    #[test]
    fn parse_lowercase_token_normalized() {
        let sc = Shortcut::parse("ctrl+shift+z");
        assert_eq!(sc.key, PhysicalKey::Char('Z'));
        assert!(sc.mods.ctrl && sc.mods.shift);
    }

    #[test]
    fn parse_special_keys() {
        assert_eq!(Shortcut::parse("Tab").key, PhysicalKey::Tab);
        assert_eq!(Shortcut::parse("Shift+Tab").key, PhysicalKey::Tab);
        assert_eq!(Shortcut::parse("Up").key, PhysicalKey::ArrowUp);
        assert_eq!(Shortcut::parse("ArrowDown").key, PhysicalKey::ArrowDown);
        assert_eq!(Shortcut::parse("Delete").key, PhysicalKey::Delete);
        assert_eq!(Shortcut::parse("F12").key, PhysicalKey::F(12));
        assert_eq!(Shortcut::parse("Cmd+Q").key, PhysicalKey::Char('Q'));
        assert!(Shortcut::parse("Cmd+Q").mods.logo);
    }

    #[test]
    fn shortcut_matches_pressed_with_mods() {
        let sc = Shortcut::parse("Ctrl+Z");
        let ev = key(PhysicalKey::Char('Z'));
        let mods = Modifiers { ctrl: true, ..Modifiers::empty() };
        assert!(sc.matches(&ev, mods));

        // released は match しない
        let mut released = ev.clone();
        released.state = ElementState::Released;
        assert!(!sc.matches(&released, mods));

        // 修飾不一致は match しない
        assert!(!sc.matches(&ev, Modifiers::empty()));
        let other = Modifiers { ctrl: true, shift: true, ..Modifiers::empty() };
        assert!(!sc.matches(&ev, other));
    }

    #[test]
    fn map_returns_first_match() {
        let mut m = ShortcutMap::new();
        m.bind("undo", "Ctrl+Z");
        m.bind("redo", "Ctrl+Shift+Z");
        let mods_ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let mods_ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::empty() };
        assert_eq!(
            m.matches(&key(PhysicalKey::Char('Z')), mods_ctrl),
            Some("undo"),
        );
        assert_eq!(
            m.matches(&key(PhysicalKey::Char('Z')), mods_ctrl_shift),
            Some("redo"),
        );
    }

    #[test]
    fn unbind_removes_all_aliases() {
        let mut m = ShortcutMap::with_default_bindings();
        // redo は Ctrl+Shift+Z と Ctrl+Y の 2 alias 登録されている
        let mods_ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let mods_ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::empty() };
        assert_eq!(
            m.matches(&key(PhysicalKey::Char('Y')), mods_ctrl),
            Some("redo"),
        );
        m.unbind("redo");
        assert!(m.matches(&key(PhysicalKey::Char('Y')), mods_ctrl).is_none());
        assert!(m.matches(&key(PhysicalKey::Char('Z')), mods_ctrl_shift).is_none());
    }

    #[test]
    fn rebind_replaces_keep_others() {
        let mut m = ShortcutMap::with_default_bindings();
        m.rebind("save", "Ctrl+Shift+S");
        let mods_ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
        let mods_ctrl_shift = Modifiers { ctrl: true, shift: true, ..Modifiers::empty() };
        // 旧 Ctrl+S はもう save にマッチしない
        assert!(m.matches(&key(PhysicalKey::Char('S')), mods_ctrl).is_none());
        // 新 Ctrl+Shift+S が save にマッチ (save_as より先勝ち or 同列)
        // ※ rebind は entries の末尾に追加される。save_as の Ctrl+Shift+S が先に登録されている。
        // よって matches は "save_as" を返す。これは「先勝ち」の仕様通り。
        let n = m.matches(&key(PhysicalKey::Char('S')), mods_ctrl_shift);
        assert!(n == Some("save_as") || n == Some("save"), "got {n:?}");
    }

    #[test]
    fn display_for_returns_canonical_string() {
        let mut m = ShortcutMap::new();
        m.bind("undo", "ctrl+z");
        assert_eq!(m.display_for("undo").as_deref(), Some("Ctrl+Z"));
        m.bind("redo", "Ctrl+Shift+Z");
        assert_eq!(m.display_for("redo").as_deref(), Some("Ctrl+Shift+Z"));
    }

    // -------- M9 P0-1: 記号キー受理 + try_parse --------

    #[test]
    fn parse_punctuation_keys() {
        // 11 種すべて Char(c) として解釈される (modifier なし)
        for (spec, expected_char) in [
            ("/", '/'),
            (";", ';'),
            (",", ','),
            (".", '.'),
            ("-", '-'),
            ("=", '='),
            ("[", '['),
            ("]", ']'),
            ("\\", '\\'),
            ("'", '\''),
            ("`", '`'),
        ] {
            let sc = Shortcut::parse(spec);
            assert_eq!(sc.key, PhysicalKey::Char(expected_char), "spec {spec:?}");
            assert!(sc.mods.is_empty(), "spec {spec:?} should have no modifiers");
        }
    }

    #[test]
    fn try_parse_succeeds_for_punctuation_with_modifier() {
        let sc = Shortcut::try_parse("Ctrl+Shift+/").expect("valid");
        assert_eq!(sc.key, PhysicalKey::Char('/'));
        assert!(sc.mods.ctrl && sc.mods.shift);
    }

    #[test]
    fn try_parse_returns_error_for_unknown_token() {
        let err = Shortcut::try_parse("Ctrl+Foo").expect_err("unknown token");
        assert_eq!(err.spec, "Ctrl+Foo");
        assert!(err.reason.contains("unknown key"), "got {err}");
    }

    #[test]
    fn try_parse_returns_error_for_empty_token() {
        let err = Shortcut::try_parse("Ctrl++Z").expect_err("empty token");
        assert_eq!(err.spec, "Ctrl++Z");
        assert_eq!(err.reason, "empty token");
    }

    #[test]
    fn try_parse_returns_error_for_no_key() {
        let err = Shortcut::try_parse("Ctrl+Shift").expect_err("no key");
        assert_eq!(err.spec, "Ctrl+Shift");
        assert_eq!(err.reason, "no key");
    }

    #[test]
    fn try_parse_returns_error_for_multiple_keys() {
        let err = Shortcut::try_parse("A+B").expect_err("two keys");
        assert_eq!(err.reason, "multiple keys");
    }

    #[test]
    fn parse_panics_for_unknown_token() {
        // const literal 用 path: 不正 spec は panic する
        let result = std::panic::catch_unwind(|| Shortcut::parse("Ctrl+???"));
        assert!(result.is_err(), "expected panic on unknown token");
    }

    #[test]
    fn display_for_punctuation_round_trips() {
        let mut m = ShortcutMap::new();
        m.bind("toggle_help", "Shift+/");
        assert_eq!(m.display_for("toggle_help").as_deref(), Some("Shift+/"));
        m.bind("focus_next_pane", "`");
        assert_eq!(m.display_for("focus_next_pane").as_deref(), Some("`"));
    }

    #[test]
    fn punctuation_shortcut_matches_key_event() {
        let sc = Shortcut::parse("Shift+/");
        let ev = key(PhysicalKey::Char('/'));
        let mods = Modifiers { shift: true, ..Modifiers::empty() };
        assert!(sc.matches(&ev, mods));
    }
}
