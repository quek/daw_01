//! 変調ソースの**値面** — `ModSource::id` でアドレスする (アーキ不変条件 1)。
//!
//! `docs/plan_rmd_88_89_cross_modulation.md` §4-2。
//!
//! 旧実装は「`Song::mod_sources` の**位置**」で値を引いていた。位置は削除・並べ替えで
//! 動くので、プロセス境界 (shmem `AudioBridge`) と永続的な派生物 (`.modenv` sidecar) を
//! またいだ瞬間に「engine が書いた slot」と「GUI が読む slot」がずれる
//! — 変調が別のソースの値で動く。値と id を**同じ面に載せて**運ぶことで、その齟齬を
//! 構造的に起こせなくする。
//!
//! 面は 2 形態:
//! - [`ModPlane`] — 所有型 (`Vec`)。GUI の poll 先 / export の snapshot / sidecar の読み出し先。
//!   RT では `clear()` + `push()` で使い回すので確保は起きない。
//! - [`ModPlaneRef`] — `Copy` な借用ビュー。RT パス (worker の raw pointer 越しを含む) は
//!   こちらを回す。

/// 所有型の値面。`ids[i]` と `values[i]` が対。
///
/// slot の並びは engine の compile 順 (= `Song::mod_sources` を
/// `MAX_MOD_SOURCES` で切ったもの) だが、**読み手は並びに依存してはいけない** —
/// 引くのは常に [`ModPlane::scalar`] (id 引き)。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModPlane {
    ids: Vec<u32>,
    values: Vec<f32>,
}

impl ModPlane {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            ids: Vec::with_capacity(n),
            values: Vec::with_capacity(n),
        }
    }

    /// 中身を空にする (capacity は保つ = RT で確保が起きない)。
    pub fn clear(&mut self) {
        self.ids.clear();
        self.values.clear();
    }

    /// 1 slot 追加する。`id == 0` (未採番 sentinel) も受けるが引けはしない。
    pub fn push(&mut self, id: u32, value: f32) {
        self.ids.push(id);
        self.values.push(value);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// `source_id` のスカラー。未知の id は `0.0` (= 変調なし)。
    #[must_use]
    #[inline]
    pub fn scalar(&self, source_id: u32) -> f32 {
        self.as_ref().scalar(source_id)
    }

    #[must_use]
    #[inline]
    pub fn as_ref(&self) -> ModPlaneRef<'_> {
        ModPlaneRef {
            ids: &self.ids,
            values: &self.values,
        }
    }

    /// `ids` を保ったまま値だけ差し替える (sidecar の 1 行読み出し用)。
    /// `values.len()` が `ids.len()` と違うときは足りない側を `0.0` で埋める。
    pub fn set_values(&mut self, values: &[f32]) {
        self.values.clear();
        self.values.extend_from_slice(values);
        self.values.resize(self.ids.len(), 0.0);
    }

    /// `ids` を丸ごと入れ替える (値は 0 で初期化)。
    pub fn reset_ids(&mut self, ids: &[u32]) {
        self.ids.clear();
        self.ids.extend_from_slice(ids);
        self.values.clear();
        self.values.resize(ids.len(), 0.0);
    }
}

/// `Copy` な借用ビュー。RT パスはこれを回す (`&[f32]` を回していた旧経路の置換)。
///
/// `ids` と `values` の長さが違う場合は短い方までが有効 ([`ModPlaneRef::scalar`] が
/// `values.get()` で弾く) — worker の raw pointer 再構成のように長さが独立に届く
/// 経路があるので、不一致を panic ではなく「引けない」に倒す。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ModPlaneRef<'a> {
    pub ids: &'a [u32],
    pub values: &'a [f32],
}

impl<'a> ModPlaneRef<'a> {
    #[must_use]
    pub const fn new(ids: &'a [u32], values: &'a [f32]) -> Self {
        Self { ids, values }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() || self.values.is_empty()
    }

    /// `source_id` のスカラー。未知の id は `0.0` (= 変調なし)。
    ///
    /// `ids` は最大 `MAX_MOD_SOURCES` (= 64) 要素の連続した `u32` なので、
    /// 線形走査でも 1 cache line 数本ぶん。`Song::mod_sources` (名前 `String` と
    /// `Vec` を抱えた太い struct) を走査していた旧 `source_scalar` と違い、
    /// per-sample 経路に乗せてよい形。
    #[must_use]
    #[inline]
    pub fn scalar(&self, source_id: u32) -> f32 {
        let mut i = 0;
        while i < self.ids.len() {
            if self.ids[i] == source_id {
                return self.values.get(i).copied().unwrap_or(0.0);
            }
            i += 1;
        }
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **位置ではなく id で引く。** slot の並びが変わっても同じソースの値が返る
    /// (これが崩れると、ソースを 1 つ消しただけで変調が別のソースの値で動く)。
    #[test]
    fn 並べ替えても同じ_id_は同じ値を返す() {
        let mut a = ModPlane::default();
        a.push(7, 0.25);
        a.push(3, 0.75);
        let mut b = ModPlane::default();
        b.push(3, 0.75);
        b.push(7, 0.25);
        assert_eq!(a.scalar(7), b.scalar(7));
        assert_eq!(a.scalar(3), b.scalar(3));
        // 未知の id / 未採番 sentinel は 0 (= 変調なし)。
        assert_eq!(a.scalar(999), 0.0);
        assert_eq!(a.scalar(0), 0.0);
    }
}
