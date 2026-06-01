//! `DocState` — TSF text store の **キャッシュ snapshot + 編集キュー** (COM 非依存の純粋ロジック)。
//!
//! Windows TSF の `ITextStoreACP` は長命 COM オブジェクトで、IME (rtry / MS-IME) が
//! メッセージポンプ中の任意タイミングで `GetText` / `GetSelection` / `SetText` 等を **同期呼び出し**
//! する。一方 gui_01 の `text_input` は immediate-mode で `frame()` 中にしか存在しない。
//! そのギャップを埋めるのがこの `DocState`:
//!
//! - **UI → store (publish)**: `text_input` が focus 中、毎フレーム [`DocState::publish`] で
//!   `(text, selection, caret)` を更新する。
//! - **store → UI (drain)**: IME が `set_text_acp` / `set_selection_acp` 等で行った編集は
//!   `pending_edits` に byte 空間の [`ImeTextEdit`] として積まれ、次フレームの `frame()` 先頭で
//!   [`DocState::take_pending_edits`] により `text_input` へ流れる。
//!
//! **echo suppression**: IME 自身の編集を widget が echo back しても、store は IME に
//! `OnTextChange`/`OnSelectionChange` を返さない (= 無限ループ防止)。`set_*_acp` が `text`/`sel`
//! cache を即時更新するので、widget の echo は cache と一致し通知が立たない (app 起因の変化だけ通知)。
//!
//! COM 型を一切含まないので全 [`crate::acp_map`] 同様に単体テストできる (COM shim
//! (`text_store.rs`, Windows 限定) はこの struct を `Rc<RefCell<>>` で包んで sink を足すだけ)。

// ACP (UTF-16 unit) / byte offset は text 長で bound されるので i32 への cast で wrap しない
// (workspace は cast_possible_truncation/precision_loss/sign_loss を許容済み、これも同カテゴリ)。
#![allow(clippy::cast_possible_wrap)]

use std::ops::Range;

use crate::acp_map::AcpMap;
use crate::text_document::{ImeTextEdit, RectPx, TextDocument};

/// app 起因の変化を IME (sink) に通知すべきか示すフラグ束。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Notify {
    /// テキストが変わった → `ITextStoreACPSink::OnTextChange`。
    pub text: bool,
    /// 選択が変わった → `OnSelectionChange`。
    pub selection: bool,
    /// caret 位置が変わった → `OnLayoutChange` (NOLAYOUT だった `GetTextExt` を再試行させる)。
    pub layout: bool,
}

impl Notify {
    /// 何も通知不要なら true。
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.text && !self.selection && !self.layout
    }
}

/// TSF text store のキャッシュ状態 (COM 非依存)。
#[derive(Debug, Default)]
pub struct DocState {
    // --- app が publish した snapshot (= widget が今表示している真値) ---
    /// UTF-8 全文。
    text: String,
    /// ACP (UTF-16) 形式 (`GetText` のコピー元)。`text` から `rebuild` で同期。
    text_utf16: Vec<u16>,
    /// ACP ⇔ byte 索引。
    map: AcpMap,
    /// 正規化済み byte 選択範囲 (`start <= end`)。
    sel: Range<usize>,
    /// caret が選択の **手前端** にあるか (cursor < anchor)。ACP の active-end 表現に使う。
    sel_reversed: bool,
    /// 候補ウィンドウ配置用 caret rect (物理 px)。
    caret: RectPx,
    /// text field が focus 中で publish されているか (false = store 空 = IME 非アクティブ)。
    active: bool,

    // --- IME → widget へ返す編集 (byte 空間、FIFO) ---
    pending_edits: Vec<ImeTextEdit>,

    /// 次に sink へ流すべき通知。
    notify: Notify,
}

impl DocState {
    /// 空の store。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `text` から `text_utf16` と `map` を再構築する。
    fn rebuild(&mut self) {
        self.text_utf16 = self.text.encode_utf16().collect();
        self.map = AcpMap::build(&self.text);
    }

    // ============================== UI → store ==============================

    /// app (focused text_input) の 1 フレーム snapshot を取り込む。`None` で focus 喪失。
    ///
    /// IME 自身の編集は、`set_text_acp`/`set_selection_acp` が `self.text`/`self.sel` を即時更新
    /// するため、widget が echo back しても `doc.text == self.text` / `new_sel == self.sel` で
    /// 一致し通知が立たない (= storm 防止)。app 起因の変化のときだけ sink へ通知する。
    pub fn publish(&mut self, doc: Option<&TextDocument>) {
        let Some(doc) = doc else {
            // focus 喪失: store を空にして IME を非アクティブ化。
            if self.active {
                self.notify.text = true;
                self.notify.selection = true;
            }
            self.active = false;
            if !self.text.is_empty() {
                self.text.clear();
                self.rebuild();
            }
            self.sel = 0..0;
            self.sel_reversed = false;
            return;
        };

        let (anchor, cursor) = doc.selection;
        let len = doc.text.len();
        let lo = anchor.min(cursor).min(len);
        let hi = anchor.max(cursor).min(len);
        let new_sel = lo..hi;
        let reversed = cursor < anchor;

        // store の cache 値と異なるときだけ通知 (IME 編集の echo は cache と一致して無通知)。
        if doc.text != self.text {
            self.notify.text = true;
        }
        if new_sel != self.sel {
            self.notify.selection = true;
        }
        if doc.caret_rect != self.caret {
            self.notify.layout = true;
        }

        if doc.text != self.text {
            self.text.clone_from(&doc.text);
            self.rebuild();
        }
        self.sel = new_sel;
        self.sel_reversed = reversed;
        self.caret = doc.caret_rect;

        let was_active = self.active;
        self.active = true;
        if !was_active {
            self.notify.text = true;
            self.notify.selection = true;
        }
    }

    /// 溜まった sink 通知を取り出してクリアする (COM shink が sink へ転送する)。
    pub fn take_notify(&mut self) -> Notify {
        std::mem::take(&mut self.notify)
    }

    /// IME が行った編集を byte 空間で取り出す (`frame()` 先頭で widget に流す)。
    pub fn take_pending_edits(&mut self) -> Vec<ImeTextEdit> {
        std::mem::take(&mut self.pending_edits)
    }

    // ============================== store 読み (ACP) ==============================

    /// text field が focus 中で publish されているか。
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// caret rect (物理 px)。
    #[must_use]
    pub fn caret(&self) -> RectPx {
        self.caret
    }

    /// UTF-16 unit 数 (= `GetEndACP`)。
    #[must_use]
    pub fn end_acp(&self) -> i32 {
        self.map.len16() as i32
    }

    /// 選択を ACP で返す `(acp_start, acp_end, reversed)` (`acp_start <= acp_end`)。
    #[must_use]
    pub fn selection_acp(&self) -> (i32, i32, bool) {
        let s = self.map.byte_to_acp(self.sel.start) as i32;
        let e = self.map.byte_to_acp(self.sel.end) as i32;
        (s, e, self.sel_reversed)
    }

    /// ACP 範囲 `[acp_start, acp_end)` の UTF-16 slice (clamp 済み)。`GetText` のコピー元。
    /// `acp_end < 0` は「末尾まで」。
    #[must_use]
    pub fn text_utf16_range(&self, acp_start: i32, acp_end: i32) -> &[u16] {
        let len16 = self.map.len16();
        let s = (acp_start.max(0) as usize).min(len16);
        let e = if acp_end < 0 {
            len16
        } else {
            (acp_end as usize).min(len16)
        }
        .max(s);
        &self.text_utf16[s..e]
    }

    // ============================== store 書き (ACP) ==============================

    /// `SetText`: ACP 範囲 `[acp_start, acp_end)` を `new_utf16` で置換する。
    /// 戻り値は `TS_TEXTCHANGE` 相当の `(acp_start, acp_old_end, acp_new_end)`。
    pub fn set_text_acp(&mut self, acp_start: i32, acp_end: i32, new_utf16: &[u16]) -> (i32, i32, i32) {
        let len16 = self.map.len16();
        let s = (acp_start.max(0) as usize).min(len16);
        let e = (acp_end.max(0) as usize).clamp(s, len16);

        // 置換前の byte offset (old map)。
        let lo = self.map.acp_to_byte(s);
        let hi = self.map.acp_to_byte(e);
        let new_text = String::from_utf16_lossy(new_utf16);
        let new_cursor_byte = lo + new_text.len();

        self.text.replace_range(lo..hi, &new_text);
        self.rebuild();
        self.sel = new_cursor_byte..new_cursor_byte;
        self.sel_reversed = false;

        self.pending_edits.push(ImeTextEdit::Replace {
            start_byte: lo,
            end_byte: hi,
            text: new_text,
            new_cursor: new_cursor_byte,
        });

        (s as i32, e as i32, (s + new_utf16.len()) as i32)
    }

    /// `SetSelection`: ACP 範囲で選択を更新する。`reversed` は caret が手前端か。
    pub fn set_selection_acp(&mut self, acp_start: i32, acp_end: i32, reversed: bool) {
        let len16 = self.map.len16();
        let a = (acp_start.max(0) as usize).min(len16);
        let b = (acp_end.max(0) as usize).min(len16);
        let lo_byte = self.map.acp_to_byte(a.min(b));
        let hi_byte = self.map.acp_to_byte(a.max(b));
        self.sel = lo_byte..hi_byte;
        self.sel_reversed = reversed;

        let (anchor_byte, cursor_byte) = if reversed {
            (hi_byte, lo_byte)
        } else {
            (lo_byte, hi_byte)
        };
        self.pending_edits
            .push(ImeTextEdit::SetSelection { anchor_byte, cursor_byte });
    }

    /// `InsertTextAtSelection` (実挿入): 現在の選択を `new_utf16` で置換する。
    /// 戻り値 `(acp_start, acp_end, (textchange tuple))`。
    pub fn insert_at_selection_acp(&mut self, new_utf16: &[u16]) -> (i32, i32, (i32, i32, i32)) {
        let (s, e, _rev) = self.selection_acp();
        let change = self.set_text_acp(s, e, new_utf16);
        let start = s;
        let end = s + new_utf16.len() as i32;
        (start, end, change)
    }

    /// `InsertTextAtSelection(TS_IAS_QUERYONLY)`: 挿入せず現在選択の ACP 範囲だけ返す。
    #[must_use]
    pub fn query_insert_at_selection(&self) -> (i32, i32) {
        let (s, e, _) = self.selection_acp();
        (s, e)
    }
}

#[cfg(test)]
mod tests {
    use super::{DocState, Notify};
    use crate::text_document::{ImeTextEdit, RectPx, TextDocument};

    fn doc(text: &str, sel: (usize, usize)) -> TextDocument {
        TextDocument {
            text: text.to_string(),
            selection: sel,
            caret_rect: RectPx::default(),
        }
    }

    /// `DocState::new()` 直後 (publish 前) に store 読み取りが panic しないこと
    /// (2026-06-01 実機起動で `republish` の `end_acp()` が AcpMap underflow した回帰)。
    #[test]
    fn fresh_state_reads_do_not_panic() {
        let d = DocState::new();
        assert!(!d.active());
        assert_eq!(d.end_acp(), 0);
        assert_eq!(d.selection_acp(), (0, 0, false));
        assert!(d.text_utf16_range(0, -1).is_empty());
    }

    #[test]
    fn first_publish_activates_and_notifies() {
        let mut d = DocState::new();
        assert!(!d.active());
        d.publish(Some(&doc("abc", (0, 3))));
        assert!(d.active());
        let n = d.take_notify();
        assert!(n.text && n.selection, "初回 publish は text+selection を通知");
        assert_eq!(d.end_acp(), 3);
        assert_eq!(d.selection_acp(), (0, 3, false));
    }

    #[test]
    fn ime_edit_round_trips_without_storm() {
        let mut d = DocState::new();
        d.publish(Some(&doc("きしゃ", (9, 9)))); // cursor 末尾 (各 3 byte)
        let _ = d.take_notify();

        // IME がカーソル前 [0..9] を "汽車" に置換 (ACP 0..3、UTF-16 2 unit)。
        let new16: Vec<u16> = "汽車".encode_utf16().collect();
        let change = d.set_text_acp(0, 3, &new16);
        assert_eq!(change, (0, 3, 2), "TS_TEXTCHANGE (start, oldEnd, newEnd)");

        // widget へ流れる byte 空間編集。
        let edits = d.take_pending_edits();
        assert_eq!(
            edits,
            vec![ImeTextEdit::Replace {
                start_byte: 0,
                end_byte: 9,
                text: "汽車".to_string(),
                new_cursor: 6,
            }]
        );
        // IME 編集自体は sink へ通知しない。
        assert_eq!(d.take_notify(), Notify::default());

        // widget が次フレームで echo back → 通知が立たない (storm 防止)。
        d.publish(Some(&doc("汽車", (6, 6))));
        assert_eq!(d.take_notify(), Notify::default(), "IME 編集の echo は無通知");
    }

    #[test]
    fn user_typed_change_notifies() {
        let mut d = DocState::new();
        d.publish(Some(&doc("abc", (3, 3))));
        let _ = d.take_notify();
        // ユーザが 'd' を打った (IME 由来でない app 変化)。
        d.publish(Some(&doc("abcd", (4, 4))));
        let n = d.take_notify();
        assert!(n.text, "app 起因の text 変化は通知");
        assert!(n.selection);
    }

    #[test]
    fn focus_loss_deactivates_once() {
        let mut d = DocState::new();
        d.publish(Some(&doc("abc", (0, 0))));
        let _ = d.take_notify();
        d.publish(None);
        assert!(!d.active());
        assert!(!d.take_notify().is_empty(), "focus 喪失で 1 度通知");
        d.publish(None);
        assert!(d.take_notify().is_empty(), "2 度目の None は無通知");
        assert_eq!(d.end_acp(), 0);
    }

    #[test]
    fn get_text_range_over_cjk() {
        let mut d = DocState::new();
        d.publish(Some(&doc("aあ😀b", (0, 0)))); // a(1u) あ(1u) 😀(2u) b(1u) = 5 unit
        assert_eq!(d.end_acp(), 5);
        // ShiftStart(-3) 相当: [2..5) = "😀b"。
        let slice = d.text_utf16_range(2, 5);
        assert_eq!(String::from_utf16_lossy(slice), "😀b");
        // acp_end<0 = 末尾まで。
        let all = d.text_utf16_range(0, -1);
        assert_eq!(String::from_utf16_lossy(all), "aあ😀b");
    }

    #[test]
    fn set_selection_pushes_byte_edit() {
        let mut d = DocState::new();
        d.publish(Some(&doc("あいう", (0, 0)))); // 各 1 unit / 3 byte
        let _ = d.take_notify();
        d.set_selection_acp(1, 2, false); // ACP [1,2) = "い"
        let edits = d.take_pending_edits();
        assert_eq!(
            edits,
            vec![ImeTextEdit::SetSelection { anchor_byte: 3, cursor_byte: 6 }]
        );
        assert_eq!(d.selection_acp(), (1, 2, false));
    }

    #[test]
    fn insert_at_selection_replaces_selection() {
        let mut d = DocState::new();
        d.publish(Some(&doc("abc", (0, 3)))); // 全選択
        let _ = d.take_notify();
        let ins: Vec<u16> = "XY".encode_utf16().collect();
        let (start, end, change) = d.insert_at_selection_acp(&ins);
        assert_eq!((start, end), (0, 2));
        assert_eq!(change, (0, 3, 2));
        let edits = d.take_pending_edits();
        assert_eq!(
            edits,
            vec![ImeTextEdit::Replace {
                start_byte: 0,
                end_byte: 3,
                text: "XY".to_string(),
                new_cursor: 2,
            }]
        );
    }
}
