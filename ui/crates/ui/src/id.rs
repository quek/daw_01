//! ウィジェット ID — 親パスのハッシュと組み合わせて一意性を担保する。
//!
//! ユーザの Model 型に `Hash` を要求しないため、ID 生成は ID として渡された値の
//! `Hash` 実装だけを使う。Model のフィールドは触らない。

use std::hash::{Hash, Hasher};

/// 親階層を畳み込んで生成される 64bit 識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WidgetId(pub u64);

impl WidgetId {
    pub const ROOT: Self = Self(0xC0FFEE);

    /// 親 ID と任意のハッシュ可能な seed から子 ID を作る。
    #[must_use]
    pub fn child<H: Hash>(self, seed: H) -> Self {
        let mut hasher = ahash_lite::AHasher::new_with_seed(self.0);
        seed.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// 軽量な決定論的ハッシュ。`std::hash::DefaultHasher` は実装が変わり得るので自前で固定する。
mod ahash_lite {
    use std::hash::Hasher;

    /// FNV-1a 64-bit。子ID 生成と input_hash 用途で十分。
    pub struct AHasher {
        state: u64,
    }

    impl AHasher {
        pub fn new_with_seed(seed: u64) -> Self {
            Self { state: 0xcbf29ce484222325_u64.wrapping_add(seed) }
        }
    }

    impl Hasher for AHasher {
        fn finish(&self) -> u64 {
            self.state
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.state ^= u64::from(*b);
                self.state = self.state.wrapping_mul(0x100000001b3);
            }
        }
    }
}
