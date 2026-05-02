//! M8 Phase 29: history stack — undo / redo を扱う。
//!
//! 設計:
//! - `Edit::Undoable { forward, inverse, label }` を `Edit::with_inverse(...)` で作成
//! - `UiHost::frame` が apply 時に `Undoable` を見つけたら forward を実行 + history へ push
//! - `Ui::request_undo()` で frame 末尾に inverse を実行 + redo stack へ移動
//! - `Ui::request_redo()` で redo stack の forward を再実行 + history へ復帰
//!
//! history は ring buffer (default capacity 100、`UiHost::with_history_capacity(n)` で変更)。
//! 新規 push のたびに redo stack はクリア (DAW 標準動作)。
//!
//! no-Clone 制約: `Arc<dyn Fn>` で forward / inverse を保持するので、ユーザ Model 型に
//! `Clone` を要求しない。スナップショットコピーも不要。

use std::collections::VecDeque;
use std::sync::Arc;

/// 1 step の undo/redo entry。
pub struct HistoryEntry<M: ?Sized + 'static> {
    pub forward: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
    pub inverse: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
    pub label: &'static str,
}

impl<M: ?Sized + 'static> HistoryEntry<M> {
    pub fn new(
        forward: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
        inverse: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
        label: &'static str,
    ) -> Self {
        Self { forward, inverse, label }
    }
}

impl<M: ?Sized + 'static> std::fmt::Debug for HistoryEntry<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HistoryEntry({})", self.label)
    }
}

/// undo / redo stack (ring buffer)。
pub struct HistoryStack<M: ?Sized + 'static> {
    undo: VecDeque<HistoryEntry<M>>,
    redo: VecDeque<HistoryEntry<M>>,
    capacity: usize,
}

impl<M: ?Sized + 'static> HistoryStack<M> {
    /// `capacity` は最大 undo step 数 (default 100、0 で無効化)。
    pub fn new(capacity: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            capacity,
        }
    }

    /// 新しい entry を push (= 直近 forward 実行直後)。redo stack はクリア。
    /// capacity 超過時は最も古い entry から削除。
    pub fn push(&mut self, entry: HistoryEntry<M>) {
        if self.capacity == 0 {
            return;
        }
        self.redo.clear();
        self.undo.push_back(entry);
        while self.undo.len() > self.capacity {
            self.undo.pop_front();
        }
    }

    /// undo 1 step。inverse を model に適用、entry は redo に移動。成功なら label を返す。
    pub fn undo(&mut self, model: &mut M) -> Option<&'static str> {
        let entry = self.undo.pop_back()?;
        (entry.inverse)(model);
        let label = entry.label;
        self.redo.push_back(entry);
        Some(label)
    }

    /// redo 1 step。forward を model に適用、entry は undo に復帰。成功なら label を返す。
    pub fn redo(&mut self, model: &mut M) -> Option<&'static str> {
        let entry = self.redo.pop_back()?;
        (entry.forward)(model);
        let label = entry.label;
        self.undo.push_back(entry);
        Some(label)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 直近 undo 候補のラベル (menu の "Undo (fader)" 表記用)。
    pub fn undo_label(&self) -> Option<&'static str> {
        self.undo.back().map(|e| e.label)
    }

    /// 直近 redo 候補のラベル。
    pub fn redo_label(&self) -> Option<&'static str> {
        self.redo.back().map(|e| e.label)
    }

    /// 全クリア (新規 project 開始時など)。
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// capacity を変更。縮小時は最古から削除。
    pub fn set_capacity(&mut self, n: usize) {
        self.capacity = n;
        while self.undo.len() > n {
            self.undo.pop_front();
        }
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

impl<M: ?Sized + 'static> Default for HistoryStack<M> {
    /// default capacity は 100 step。
    fn default() -> Self {
        Self::new(100)
    }
}

impl<M: ?Sized + 'static> std::fmt::Debug for HistoryStack<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryStack")
            .field("undo_len", &self.undo.len())
            .field("redo_len", &self.redo.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;

    struct M {
        v: i32,
    }

    fn make_undoable(label: &'static str, target: i32, prev: i32) -> HistoryEntry<M> {
        HistoryEntry::new(
            Arc::new(move |m: &mut M| m.v = target),
            Arc::new(move |m: &mut M| m.v = prev),
            label,
        )
    }

    #[test]
    fn push_undo_redo_round_trip() {
        let mut h: HistoryStack<M> = HistoryStack::new(10);
        let mut m = M { v: 0 };

        // forward: 0 -> 5, inverse: 5 -> 0 を想定。push 直前に forward 適用済みを模擬。
        let entry = make_undoable("set5", 5, 0);
        (entry.forward)(&mut m);
        h.push(entry);
        assert_eq!(m.v, 5);

        assert!(h.can_undo());
        assert!(!h.can_redo());

        h.undo(&mut m);
        assert_eq!(m.v, 0);
        assert!(!h.can_undo());
        assert!(h.can_redo());

        h.redo(&mut m);
        assert_eq!(m.v, 5);
        assert!(h.can_undo());
        assert!(!h.can_redo());
    }

    #[test]
    fn push_clears_redo() {
        let mut h: HistoryStack<M> = HistoryStack::new(10);
        let mut m = M { v: 0 };

        let e1 = make_undoable("set5", 5, 0);
        (e1.forward)(&mut m);
        h.push(e1);
        h.undo(&mut m);
        assert!(h.can_redo());

        // 新しい push で redo がクリアされる (DAW 標準: 過去の redo は消える)
        let e2 = make_undoable("set10", 10, 0);
        (e2.forward)(&mut m);
        h.push(e2);
        assert!(!h.can_redo());
        assert!(h.can_undo());
    }

    #[test]
    fn capacity_truncates_oldest() {
        let mut h: HistoryStack<M> = HistoryStack::new(2);
        h.push(make_undoable("a", 1, 0));
        h.push(make_undoable("b", 2, 1));
        h.push(make_undoable("c", 3, 2));
        // a は弾かれて b, c だけ残る
        assert_eq!(h.undo_len(), 2);
        assert_eq!(h.undo_label(), Some("c"));
    }

    #[test]
    fn capacity_zero_disables_history() {
        let mut h: HistoryStack<M> = HistoryStack::new(0);
        h.push(make_undoable("a", 1, 0));
        assert!(!h.can_undo());
    }

    #[test]
    fn edit_with_inverse_creates_undoable() {
        let edit: Edit<M> = Edit::with_inverse(
            "set5",
            |m: &mut M| m.v = 5,
            |m: &mut M| m.v = 0,
        );
        match edit {
            Edit::Undoable { label, .. } => assert_eq!(label, "set5"),
            Edit::Mutate(_) => panic!("expected Undoable"),
        }
    }

    #[test]
    fn edit_apply_runs_forward_only() {
        let mut m = M { v: 0 };
        let edit: Edit<M> = Edit::with_inverse(
            "set5",
            |m: &mut M| m.v = 5,
            |m: &mut M| m.v = 0,
        );
        edit.apply(&mut m);
        assert_eq!(m.v, 5);
    }

    #[test]
    fn edit_label_returns_undoable_label() {
        let mutate: Edit<M> = Edit::mutate(|m: &mut M| m.v = 1);
        let undoable: Edit<M> = Edit::with_inverse(
            "L",
            |m: &mut M| m.v = 2,
            |m: &mut M| m.v = 0,
        );
        assert_eq!(mutate.label(), None);
        assert_eq!(undoable.label(), Some("L"));
    }

    #[test]
    fn multiple_undo_redo_in_sequence() {
        let mut h: HistoryStack<M> = HistoryStack::new(10);
        let mut m = M { v: 0 };

        h.push(make_undoable("a", 1, 0));
        m.v = 1;
        h.push(make_undoable("b", 2, 1));
        m.v = 2;
        h.push(make_undoable("c", 3, 2));
        m.v = 3;

        h.undo(&mut m);
        assert_eq!(m.v, 2);
        h.undo(&mut m);
        assert_eq!(m.v, 1);
        h.undo(&mut m);
        assert_eq!(m.v, 0);
        assert!(!h.can_undo());

        h.redo(&mut m);
        assert_eq!(m.v, 1);
        h.redo(&mut m);
        assert_eq!(m.v, 2);
        h.redo(&mut m);
        assert_eq!(m.v, 3);
        assert!(!h.can_redo());
    }

    #[test]
    fn set_capacity_shrinks_undo() {
        let mut h: HistoryStack<M> = HistoryStack::new(5);
        for i in 0..5 {
            h.push(make_undoable("x", i + 1, i));
        }
        assert_eq!(h.undo_len(), 5);
        h.set_capacity(2);
        assert_eq!(h.undo_len(), 2);
    }
}
