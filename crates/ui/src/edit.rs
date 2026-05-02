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
