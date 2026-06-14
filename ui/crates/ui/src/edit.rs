//! `Edit<M>` — ユーザ操作から発生する編集要求。アプリ層に渡され、apply される。
//!
//! 設計方針:
//! - メッセージ型は導入しない (`Application::Message: Clone` 伝染を防ぐため)
//! - 1 回限りの mutation は `Box<dyn FnOnce(&mut M) + 'static>` に畳み込む
//! - undoable な mutation は `Arc<dyn Fn(&mut M) + Send + Sync + 'static>` を 2 本
//!   (forward / inverse) 持つ。redo を実現するためには forward を 2 度以上実行する必要が
//!   あるので `Fn` 制約 (FnOnce では消費されて再実行できない)。
//! - `M` をジェネリックパラメータで持つことで、ユーザが定義した任意の Model 型に紐づけられる
//!
//! M8 Phase 29: `Edit::with_inverse(label, forward, inverse)` で undoable Edit を作る。
//! UiHost が apply 時に `Undoable` variant を見つけたら forward を実行 + history stack へ push
//! する。`apply()` を直接呼んだ場合 (frame_to_edits を low-level に使う場合) は forward だけ
//! 実行され、history には積まれない (この場合 user が `UiHost::history_mut()` で自前管理する)。

use std::sync::Arc;

/// 1 つの編集。
pub enum Edit<M: ?Sized + 'static> {
    /// 一回限りの mutation (history に乗らない)。fader drag の 60Hz 連続更新など、
    /// 個別 step を undoable にしたくない高頻度更新で使う。
    Mutate(Box<dyn FnOnce(&mut M) + Send + 'static>),
    /// undoable な mutation。forward / inverse を Arc で持ち、`UiHost::frame` が apply 時に
    /// forward を実行しつつ entry を history stack へ push する。
    Undoable {
        forward: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
        inverse: Arc<dyn Fn(&mut M) + Send + Sync + 'static>,
        /// menu / history パネル表記用ラベル ("fader change", "trim left" 等)。
        label: &'static str,
    },
}

impl<M: ?Sized + 'static> Edit<M> {
    /// 短縮コンストラクタ (history に乗らない)。`FnOnce` 使用可。
    pub fn mutate<F: FnOnce(&mut M) + Send + 'static>(f: F) -> Self {
        Self::Mutate(Box::new(f))
    }

    /// undoable Edit を作る。
    ///
    /// - `label`: menu / history パネル表示用の `&'static str` ("fader change" など)。
    /// - `forward`: 適用方向のクロージャ。redo 時に再実行されるので `Fn` (1 回限りでない) であること。
    /// - `inverse`: 巻き戻しクロージャ。undo 時に実行される、こちらも `Fn`。
    ///
    /// 典型的な書き方は drag 終端の値変更で:
    /// ```ignore
    /// let prev = model.fader.value;
    /// let next = displayed_value;
    /// Edit::with_inverse(
    ///     "fader",
    ///     move |m: &mut MyModel| m.fader.value = next,
    ///     move |m: &mut MyModel| m.fader.value = prev,
    /// )
    /// ```
    /// `prev` / `next` は Copy 値だけキャプチャしているので `Fn` で問題ない。大きいデータを
    /// キャプチャする場合は `Arc<...>` で wrap するとよい。
    pub fn with_inverse<F, I>(label: &'static str, forward: F, inverse: I) -> Self
    where
        F: Fn(&mut M) + Send + Sync + 'static,
        I: Fn(&mut M) + Send + Sync + 'static,
    {
        Self::Undoable {
            forward: Arc::new(forward),
            inverse: Arc::new(inverse),
            label,
        }
    }

    /// snapshot 共有付き Undoable Edit (M9 Phase 41d)。
    ///
    /// `Vec<Note>` / `Vec<f32>` / `Arc<[T]>` 級の重いデータを forward / inverse 両方の
    /// closure で共有する典型パターンを 1 関数に集約し、利用者の `Arc::clone` boilerplate を
    /// 吸収する。snapshot は library 側で `Arc` 化されて 2 closure に共有される。
    ///
    /// - `snapshot`: forward / inverse 両方が参照する不変データ (any `Send + Sync + 'static`)。
    ///   通常は対象 note 群の `Arc<[Note]>` や、編集前後の値の tuple `(prev, next)` 等。
    /// - `forward(&mut M, &S)`: 適用方向。snapshot を見て model を進める。
    /// - `restore_from(&mut M, &S)`: 巻き戻し。snapshot から model 値を復元する。
    ///
    /// # Examples
    /// ```ignore
    /// let notes_to_add: Arc<[Note]> = Arc::from(vec![n1, n2]);
    /// Edit::snapshot_inverse(
    ///     "add notes",
    ///     notes_to_add,
    ///     |m, snap| { for n in snap.iter() { m.notes.push(*n); } },
    ///     |m, snap| {
    ///         let ids: HashSet<u32> = snap.iter().map(|n| n.id).collect();
    ///         m.notes.retain(|n| !ids.contains(&n.id));
    ///     },
    /// )
    /// ```
    pub fn snapshot_inverse<S, F, R>(
        label: &'static str,
        snapshot: S,
        forward: F,
        restore_from: R,
    ) -> Self
    where
        S: Send + Sync + 'static,
        F: Fn(&mut M, &S) + Send + Sync + 'static,
        R: Fn(&mut M, &S) + Send + Sync + 'static,
    {
        let snap = Arc::new(snapshot);
        let snap_fwd = Arc::clone(&snap);
        let snap_inv = snap;
        Self::with_inverse(
            label,
            move |m| forward(m, &snap_fwd),
            move |m| restore_from(m, &snap_inv),
        )
    }

    /// アプリ側で保持している `&mut M` に対して apply。
    /// `Undoable` の場合は forward だけ実行する (history への push は呼び出し側責務)。
    pub fn apply(self, model: &mut M) {
        match self {
            Self::Mutate(f) => f(model),
            Self::Undoable { forward, .. } => forward(model),
        }
    }

    /// `Undoable` のときだけ label を返す。
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::Undoable { label, .. } => Some(label),
            Self::Mutate(_) => None,
        }
    }
}

impl<M: ?Sized + 'static> std::fmt::Debug for Edit<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mutate(_) => f.write_str("Edit::Mutate(<closure>)"),
            Self::Undoable { label, .. } => write!(f, "Edit::Undoable({label})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // M9 Phase 41d: Edit::snapshot_inverse のテスト

    struct ListModel {
        items: Vec<i32>,
    }

    #[test]
    fn snapshot_inverse_round_trip_with_arc_slice() {
        let mut m = ListModel { items: vec![1, 2, 3] };
        let snapshot: Arc<[i32]> = Arc::from([10, 20, 30]);
        let edit = Edit::snapshot_inverse(
            "add items",
            snapshot,
            |m: &mut ListModel, snap: &Arc<[i32]>| {
                for v in snap.iter() {
                    m.items.push(*v);
                }
            },
            |m: &mut ListModel, snap: &Arc<[i32]>| {
                for v in snap.iter() {
                    if let Some(pos) = m.items.iter().rposition(|x| *x == *v) {
                        m.items.remove(pos);
                    }
                }
            },
        );
        let Edit::Undoable { forward, inverse, label } = edit else {
            panic!("expected Undoable");
        };
        assert_eq!(label, "add items");
        forward(&mut m);
        assert_eq!(m.items, vec![1, 2, 3, 10, 20, 30]);
        inverse(&mut m);
        assert_eq!(m.items, vec![1, 2, 3]);
    }

    #[test]
    fn snapshot_inverse_with_tuple_pair_for_select_pattern() {
        // (prev, next) tuple を snapshot にして「state 置換 + undo」型 helper を表現する。
        struct SelectionModel {
            selected: Vec<u32>,
        }
        let mut m = SelectionModel { selected: vec![1, 2] };
        let snap: (Vec<u32>, Vec<u32>) = (vec![1, 2], vec![3, 4, 5]);
        let edit = Edit::snapshot_inverse(
            "select",
            snap,
            |m: &mut SelectionModel, s: &(Vec<u32>, Vec<u32>)| {
                m.selected.clone_from(&s.1);
            },
            |m: &mut SelectionModel, s: &(Vec<u32>, Vec<u32>)| {
                m.selected.clone_from(&s.0);
            },
        );
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!();
        };
        forward(&mut m);
        assert_eq!(m.selected, vec![3, 4, 5]);
        inverse(&mut m);
        assert_eq!(m.selected, vec![1, 2]);
    }

    #[test]
    fn snapshot_inverse_forward_is_idempotent_for_redo() {
        // forward を 2 度走らせても破綻しない (= Fn 制約 + idempotent な実装が前提)。
        struct CountModel {
            count: i32,
        }
        let mut m = CountModel { count: 0 };
        let snap: i32 = 5;
        let edit = Edit::snapshot_inverse(
            "set count",
            snap,
            |m: &mut CountModel, s: &i32| m.count = *s,
            |m: &mut CountModel, _s: &i32| m.count = 0,
        );
        let Edit::Undoable { forward, .. } = edit else { panic!() };
        forward(&mut m);
        forward(&mut m);
        assert_eq!(m.count, 5, "set は idempotent なので 2 度 apply しても結果同じ");
    }

    #[test]
    fn snapshot_inverse_send_sync_closures() {
        // type system check: snapshot_inverse の戻り Edit が Send + Sync 制約を満たすことを
        // コンパイル時に固定。
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let edit = Edit::snapshot_inverse(
            "noop",
            42_i32,
            |_m: &mut (), _s: &i32| {},
            |_m: &mut (), _s: &i32| {},
        );
        if let Edit::Undoable { forward, inverse, .. } = &edit {
            assert_send_sync(forward);
            assert_send_sync(inverse);
        }
    }
}
