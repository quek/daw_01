//! `Edit<M>` — ユーザ操作から発生する編集要求。アプリ層に渡され、apply される。
//!
//! 設計方針:
//! - メッセージ型は導入しない (`Application::Message: Clone` 伝染を防ぐため)
//! - 末端では `Box<dyn FnOnce(&mut M) + 'static>` に畳み込み、ユーザが書く apply 処理は不要
//! - `M` をジェネリックパラメータで持つことで、ユーザが定義した任意の Model 型に紐づけられる

/// 1 つの編集。`Mutate` クロージャは `'static` データのみキャプチャできる
/// (M1 設計の単純化)。借用キャプチャが必要になった段階で `Edit<'a, M>` への
/// 拡張を検討する。
pub enum Edit<M: ?Sized + 'static> {
    Mutate(Box<dyn FnOnce(&mut M) + Send + 'static>),
}

impl<M: ?Sized + 'static> Edit<M> {
    /// 短縮コンストラクタ。
    pub fn mutate<F: FnOnce(&mut M) + Send + 'static>(f: F) -> Self {
        Self::Mutate(Box::new(f))
    }

    /// アプリ側で保持している `&mut M` に対して apply。
    pub fn apply(self, model: &mut M) {
        match self {
            Self::Mutate(f) => f(model),
        }
    }
}

impl<M: ?Sized + 'static> std::fmt::Debug for Edit<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mutate(_) => f.write_str("Edit::Mutate(<closure>)"),
        }
    }
}
