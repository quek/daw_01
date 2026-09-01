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

/// buffer 1 個ぶんの **刻みごとの**値面 (r.md #89 §2.2)。
///
/// 行 = 制御刻み (64 サンプル)、列 = slot。行 `i` は **絶対 song サンプル位置**
/// `(first_tick + i) * MOD_TICK_FRAMES` **時点の**値で、その間は隣り合う 2 行の
/// 線形補間で埋める。ZOH (段) にすると刻み周期 (48kHz で 750Hz) の段差が音になって
/// 出るので、変調は必ず補間して当てる。
///
/// 「刻み境界が絶対サンプル位置に整列している」ので、行の中身は buffer の切り方に
/// 依存しない — live (device buffer 長) と書き出し (1024 固定) が同じ値を踏む。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModTickPlane {
    ids: Vec<u32>,
    /// `rows * ids.len()` の row-major。
    values: Vec<f32>,
    /// buffer 頭から **最初の刻み境界**までの frame 数。
    /// buffer 頭がちょうど境界なら [`crate::mod_graph::MOD_TICK_FRAMES`]
    /// (= 行 0 が buffer 頭の値、行 1 が 64 frame 目の値)。
    lead: u32,
}

impl ModTickPlane {
    #[must_use]
    pub fn with_capacity(sources: usize, ticks: usize) -> Self {
        Self {
            ids: Vec::with_capacity(sources),
            values: Vec::with_capacity(sources * ticks),
            lead: crate::mod_graph::MOD_TICK_FRAMES,
        }
    }

    /// 列 (= slot の id 表) を張り直し、行を空にする。確保は起きない。
    pub fn reset(&mut self, ids: &[u32], lead: u32) {
        self.ids.clear();
        self.ids.extend_from_slice(ids);
        self.values.clear();
        self.lead = lead.max(1);
    }

    /// 行を 1 本足す (`values` は `ids` と同じ並び)。長さが足りなければ 0 で埋め、
    /// 余りは捨てる (行の長さが列数と食い違った表を作らない)。
    pub fn push_row(&mut self, values: &[f32]) {
        debug_assert_eq!(values.len(), self.ids.len());
        let cols = self.ids.len();
        let n = values.len().min(cols);
        self.values.extend_from_slice(&values[..n]);
        for _ in n..cols {
            self.values.push(0.0);
        }
    }

    /// 先頭 `n` 行を捨てる (buffer をまたいで持ち越した古い刻みの掃除)。
    /// `Vec::drain` は確保しないので RT 安全。
    pub fn drop_leading_rows(&mut self, n: usize) {
        let cols = self.ids.len();
        if cols == 0 || n == 0 {
            return;
        }
        let cut = (n * cols).min(self.values.len());
        self.values.drain(..cut);
    }

    pub fn set_lead(&mut self, lead: u32) {
        self.lead = lead.max(1);
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        if self.ids.is_empty() {
            0
        } else {
            self.values.len() / self.ids.len()
        }
    }

    #[must_use]
    pub fn as_ref(&self) -> ModTickPlaneRef<'_> {
        ModTickPlaneRef {
            ids: &self.ids,
            values: &self.values,
            lead: self.lead,
        }
    }
}

/// [`ModTickPlane`] の `Copy` な借用ビュー (RT パスはこれを回す)。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ModTickPlaneRef<'a> {
    pub ids: &'a [u32],
    pub values: &'a [f32],
    /// buffer 頭から最初の刻み境界までの frame 数 (境界に乗っているなら 64)。
    pub lead: u32,
}

impl<'a> ModTickPlaneRef<'a> {
    #[must_use]
    pub const fn new(ids: &'a [u32], values: &'a [f32], lead: u32) -> Self {
        Self { ids, values, lead }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() || self.values.is_empty()
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        if self.ids.is_empty() {
            0
        } else {
            self.values.len() / self.ids.len()
        }
    }

    /// 行 `i` の面 (= その刻み境界ちょうどの値)。
    #[must_use]
    pub fn row(&self, i: usize) -> ModPlaneRef<'a> {
        let cols = self.ids.len();
        let start = i * cols;
        match self.values.get(start..start + cols) {
            Some(v) => ModPlaneRef { ids: self.ids, values: v },
            None => ModPlaneRef { ids: self.ids, values: &[] },
        }
    }

    /// `frame` を挟む 2 行と、その間の位置 `0..1`。
    #[must_use]
    #[inline]
    pub fn segment(&self, frame: u32) -> (usize, usize, f32) {
        let tick = f32::from(u16::try_from(crate::mod_graph::MOD_TICK_FRAMES).unwrap_or(64));
        if frame < self.lead {
            // buffer 頭〜最初の境界。行 0 の値へ向かって補間する材料が無いので、
            // 行 0 (= 前 buffer 末の刻み) と行 1 の間として扱う。
            let t = if self.lead == 0 {
                0.0
            } else {
                1.0 - (self.lead - frame) as f32 / tick
            };
            return (0, 1, t.clamp(0.0, 1.0));
        }
        let off = frame - self.lead;
        let i = 1 + (off / crate::mod_graph::MOD_TICK_FRAMES) as usize;
        let t = (off % crate::mod_graph::MOD_TICK_FRAMES) as f32 / tick;
        (i, i + 1, t)
    }

    /// buffer 内の **刻み区間の開始 frame** (`0, lead, lead+64, ...`)。
    /// plugin param の変調を刻みごとに送るときの frame offset。
    pub fn starts(&self, frames: u32) -> impl Iterator<Item = u32> + '_ {
        let lead = self.lead;
        (0..).map_while(move |i: u32| {
            let f = if i == 0 {
                0
            } else {
                lead.saturating_add((i - 1).saturating_mul(crate::mod_graph::MOD_TICK_FRAMES))
            };
            (f < frames).then_some(f)
        })
    }

    /// `frame` における `source_id` のスカラー (刻みの間は線形補間)。
    ///
    /// 最終行より後ろ (= この buffer で先の刻みをまだ評価していない範囲) は
    /// 最終行を保持する。呼び出し側が **buffer 末より 1 刻み先まで**評価して
    /// おけば保持区間は生じない (= live と書き出しで同じ値になる)。
    #[must_use]
    #[inline]
    pub fn scalar_at_frame(&self, source_id: u32, frame: u32) -> f32 {
        let rows = self.rows();
        if rows == 0 {
            return 0.0;
        }
        let (a, b, t) = self.segment(frame);
        let va = self.row(a.min(rows - 1)).scalar(source_id);
        if b >= rows {
            return va;
        }
        let vb = self.row(b).scalar(source_id);
        va + (vb - va) * t
    }
}
