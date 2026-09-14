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
/// slot の並びは engine の compile 順 (= `Song::mod_sources` の評価順) だが、
/// **読み手は並びに依存してはいけない** — 引くのは常に [`ModPlane::scalar`] (id 引き)。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModPlane {
    ids: Vec<u32>,
    values: Vec<f32>,
    /// r.md #89 Q9: 深さが動く変調の `ModRouting::id`。
    depth_ids: Vec<u32>,
    /// 上と対の実効深さ。空なら「深さは全部モデル値のまま」。
    depths: Vec<f32>,
}

impl ModPlane {
    /// `sources` 本の値と `depth_cols` 本の深さを確保なしで積める面。
    #[must_use]
    pub fn with_capacity(sources: usize, depth_cols: usize) -> Self {
        Self {
            ids: Vec::with_capacity(sources),
            values: Vec::with_capacity(sources),
            depth_ids: Vec::with_capacity(depth_cols),
            depths: Vec::with_capacity(depth_cols),
        }
    }

    /// 中身を空にする (capacity は保つ = RT で確保が起きない)。
    pub fn clear(&mut self) {
        self.ids.clear();
        self.values.clear();
        self.depth_ids.clear();
        self.depths.clear();
    }

    /// 深さが動く変調 1 本を追加する (r.md #89 Q9)。
    pub fn push_depth(&mut self, routing_id: u32, depth: f32) {
        self.depth_ids.push(routing_id);
        self.depths.push(depth);
    }

    /// `routing_id` の実効深さ。深さが動かない変調は `None` (モデル値を使う)。
    #[must_use]
    #[inline]
    pub fn depth(&self, routing_id: u32) -> Option<f32> {
        self.as_ref().depth(routing_id)
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
            depth_ids: &self.depth_ids,
            depths: &self.depths,
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
    /// r.md #89 Q9: 深さが動く変調の id と実効深さ (対)。空なら全部モデル値のまま。
    pub depth_ids: &'a [u32],
    pub depths: &'a [f32],
}

impl<'a> ModPlaneRef<'a> {
    #[must_use]
    pub const fn new(ids: &'a [u32], values: &'a [f32]) -> Self {
        Self { ids, values, depth_ids: &[], depths: &[] }
    }

    /// 深さの面も持つビュー (r.md #89 Q9)。
    #[must_use]
    pub const fn with_depths(
        ids: &'a [u32],
        values: &'a [f32],
        depth_ids: &'a [u32],
        depths: &'a [f32],
    ) -> Self {
        Self { ids, values, depth_ids, depths }
    }

    /// `routing_id` の実効深さ。未知の id は `None` (= モデル値のまま)。
    /// `depth_ids` は「深さを動かしている変調」だけなので通常 0〜数本。
    #[must_use]
    #[inline]
    pub fn depth(&self, routing_id: u32) -> Option<f32> {
        let mut i = 0;
        while i < self.depth_ids.len() {
            if self.depth_ids[i] == routing_id {
                return self.depths.get(i).copied();
            }
            i += 1;
        }
        None
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() || self.values.is_empty()
    }

    /// `source_id` のスカラー。未知の id は `0.0` (= 変調なし)。
    ///
    /// 線形走査 (GUI / sidecar の 1 フレーム 1 回の引き用)。RT の per-sample 経路は
    /// id 索引を持つ [`ModTickPlaneRef::scalar_at_frame_opt`] を使う。
    #[must_use]
    #[inline]
    pub fn scalar(&self, source_id: u32) -> f32 {
        self.scalar_opt(source_id).unwrap_or(0.0)
    }

    /// `source_id` のスカラー。 面に載っていない id は `None` (r.md #115: バイパス中の
    /// source は評価計画から外れて面に載らないので、 合成側はその routing を飛ばす)。
    #[must_use]
    #[inline]
    pub fn scalar_opt(&self, source_id: u32) -> Option<f32> {
        let mut i = 0;
        while i < self.ids.len() {
            if self.ids[i] == source_id {
                return self.values.get(i).copied();
            }
            i += 1;
        }
        None
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

    /// 刻み面は id 索引で列を引く: ソースが何百あっても、並びがばらばらでも、索引なしの線形引きと同じ値を返す。
    #[test]
    fn 刻み面は索引で引いても線形引きと同じ値() {
        let ids: Vec<u32> = (1..=300u32).rev().map(|i| i * 7).collect();
        let mut plane = ModTickPlane::with_capacity(ids.len(), 0, 4);
        plane.reset(&ids, &[], crate::mod_graph::MOD_TICK_FRAMES);
        let row0: Vec<f32> = (0..ids.len()).map(|c| c as f32).collect();
        let row1: Vec<f32> = (0..ids.len()).map(|c| c as f32 + 1.0).collect();
        plane.push_row(&row0, &[]);
        plane.push_row(&row1, &[]);
        let indexed = plane.as_ref();
        let linear = ModTickPlaneRef::new(indexed.ids, indexed.values, indexed.lead);
        for &id in &[7u32, 1050, 2100, 999_999, 0] {
            for frame in [0, 16, 63] {
                assert_eq!(indexed.scalar_at_frame_opt(id, frame), linear.scalar_at_frame_opt(id, frame), "id={id} frame={frame}");
            }
        }
        assert_eq!(indexed.scalar_at_frame_opt(2100, 0), Some(0.0), "id 2100 は列 0 (並びの先頭)");
        assert!(indexed.scalar_at_frame_opt(0, 0).is_none(), "未採番 sentinel は引けない");
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
    /// `(id, 列)` を id 昇順に並べた索引。per-sample 経路が id から列を引くのに使う
    /// (ソース数に上限が無いので線形走査にしない — `docs/plan_unbounded_tracks.md` §2.7)。
    col_of: Vec<(u32, u32)>,
    /// `rows * ids.len()` の row-major。
    values: Vec<f32>,
    /// r.md #89 Q9: 深さが動く変調の `ModRouting::id` (列)。
    depth_ids: Vec<u32>,
    /// `(routing id, 列)` を id 昇順に並べた索引 (`col_of` と同じ理由 — routing ごと・刻みごとに引く)。
    depth_col_of: Vec<(u32, u32)>,
    /// `rows * depth_ids.len()` の row-major。深さも刻みごとに動く。
    depths: Vec<f32>,
    /// buffer 頭から **最初の刻み境界**までの frame 数。
    /// buffer 頭がちょうど境界なら [`crate::mod_graph::MOD_TICK_FRAMES`]
    /// (= 行 0 が buffer 頭の値、行 1 が 64 frame 目の値)。
    lead: u32,
    /// r.md #117: buffer 先頭の絶対 song サンプル位置。
    first_sample: u64,
}

impl ModTickPlane {
    #[must_use]
    pub fn with_capacity(sources: usize, depth_cols: usize, ticks: usize) -> Self {
        Self {
            ids: Vec::with_capacity(sources),
            col_of: Vec::with_capacity(sources),
            values: Vec::with_capacity(sources * ticks),
            depth_ids: Vec::with_capacity(depth_cols),
            depth_col_of: Vec::with_capacity(depth_cols),
            depths: Vec::with_capacity(depth_cols * ticks),
            lead: crate::mod_graph::MOD_TICK_FRAMES,
            first_sample: 0,
        }
    }

    /// 列 (= slot の id 表) を張り直し、行を空にする。容量内なら確保は起きない
    /// (索引の並べ替えは in-place)。
    pub fn reset(&mut self, ids: &[u32], depth_ids: &[u32], lead: u32) {
        fn index(dst: &mut Vec<(u32, u32)>, ids: &[u32]) {
            dst.clear();
            #[allow(clippy::cast_possible_truncation)]
            dst.extend(ids.iter().enumerate().map(|(col, &id)| (id, col as u32)));
            dst.sort_unstable_by_key(|&(id, _)| id);
        }
        self.ids.clear();
        self.ids.extend_from_slice(ids);
        index(&mut self.col_of, ids);
        self.values.clear();
        self.depth_ids.clear();
        self.depth_ids.extend_from_slice(depth_ids);
        index(&mut self.depth_col_of, depth_ids);
        self.depths.clear();
        self.lead = lead.max(1);
    }

    /// 行を 1 本足す (`values` は `ids` と同じ並び)。長さが足りなければ 0 で埋め、
    /// 余りは捨てる (行の長さが列数と食い違った表を作らない)。
    pub fn push_row(&mut self, values: &[f32], depths: &[f32]) {
        debug_assert_eq!(values.len(), self.ids.len());
        let cols = self.ids.len();
        let n = values.len().min(cols);
        self.values.extend_from_slice(&values[..n]);
        for _ in n..cols {
            self.values.push(0.0);
        }
        // r.md #89 Q9: 深さの実効値も同じ行数で持つ (刻みごとに動くので面と対)。
        let dcols = self.depth_ids.len();
        let dn = depths.len().min(dcols);
        self.depths.extend_from_slice(&depths[..dn]);
        for _ in dn..dcols {
            self.depths.push(0.0);
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
        let dcols = self.depth_ids.len();
        if dcols > 0 {
            let dcut = (n * dcols).min(self.depths.len());
            self.depths.drain(..dcut);
        }
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
            col_of: &self.col_of,
            values: &self.values,
            depth_ids: &self.depth_ids,
            depth_col_of: &self.depth_col_of,
            depths: &self.depths,
            lead: self.lead,
            first_sample: self.first_sample,
            nodes: &[],
        }
    }

    /// r.md #117: buffer 先頭の絶対 song サンプル位置 (runner が `set_lead` と一緒に書く)。
    pub fn set_first_sample(&mut self, first_sample: u64) {
        self.first_sample = first_sample;
    }
}

/// [`ModTickPlane`] の `Copy` な借用ビュー (RT パスはこれを回す)。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ModTickPlaneRef<'a> {
    pub ids: &'a [u32],
    /// `(id, 列)` を id 昇順に並べた索引 ([`ModTickPlane`] の同名 field)。空なら `ids` を線形に引く。
    pub col_of: &'a [(u32, u32)],
    pub values: &'a [f32],
    /// r.md #89 Q9: 深さが動く変調の id と、行ごとの実効深さ。
    pub depth_ids: &'a [u32],
    /// `(routing id, 列)` を id 昇順に並べた索引。空なら `depth_ids` を線形に引く。
    pub depth_col_of: &'a [(u32, u32)],
    pub depths: &'a [f32],
    /// buffer 頭から最初の刻み境界までの frame 数 (境界に乗っているなら 64)。
    pub lead: u32,
    /// r.md #117: この buffer の先頭の **絶対 song サンプル位置** (ボイスの起点秒と ADSR の
    /// 時間軸)。 面と一緒に運ぶので runner の引数を増やさない。
    pub first_sample: u64,
    /// 列 (= plan の slot) ごとのソース ([`crate::mod_graph::ModPlan::nodes`])。per-note の経路が id から
    /// ソースの種類を引く ([`Self::source_node`]、`Song::mod_sources` を id で線形に探さない)。空なら引けない。
    pub nodes: &'a [crate::mod_graph::ModNode],
}

impl<'a> ModTickPlaneRef<'a> {
    #[must_use]
    pub const fn new(ids: &'a [u32], values: &'a [f32], lead: u32) -> Self {
        Self { ids, col_of: &[], values, depth_ids: &[], depth_col_of: &[], depths: &[], lead, first_sample: 0, nodes: &[] }
    }

    /// id → 列の索引 ([`ModTickPlane`] が持つ `(id, 列)` の id 昇順) を付けたビュー。
    #[must_use]
    pub const fn with_index(self, col_of: &'a [(u32, u32)]) -> Self {
        Self { col_of, ..self }
    }

    /// 列ごとのソース (列 = plan の slot のとき、`plan.nodes`) を付けたビュー。
    #[must_use]
    pub const fn with_nodes(self, nodes: &'a [crate::mod_graph::ModNode]) -> Self {
        Self { nodes, ..self }
    }

    /// `source_id` のソース (評価計画に載っている = 有効なソースだけ)。
    #[must_use]
    #[inline]
    pub fn source_node(&self, source_id: u32) -> Option<&'a crate::mod_graph::ModNode> {
        self.nodes.get(self.column(source_id)?)
    }

    /// `routing_id` の深さの列。
    #[inline]
    fn depth_column(&self, routing_id: u32) -> Option<usize> {
        if self.depth_col_of.is_empty() {
            return self.depth_ids.iter().position(|&id| id == routing_id);
        }
        let i = self.depth_col_of.binary_search_by_key(&routing_id, |&(id, _)| id).ok()?;
        Some(self.depth_col_of[i].1 as usize)
    }

    /// `source_id` の列。未採番 sentinel (`0`) と面に無い id は `None`。
    #[must_use]
    #[inline]
    fn column(&self, source_id: u32) -> Option<usize> {
        if source_id == 0 {
            return None;
        }
        if self.col_of.is_empty() {
            return self.ids.iter().position(|&id| id == source_id);
        }
        let i = self.col_of.binary_search_by_key(&source_id, |&(id, _)| id).ok()?;
        Some(self.col_of[i].1 as usize)
    }

    /// r.md #117: buffer 先頭の絶対 song サンプル位置。
    #[must_use]
    pub fn first_sample(&self) -> u64 {
        self.first_sample
    }

    /// 深さの面も持つビュー (r.md #89 Q9)。
    #[must_use]
    pub const fn with_depths(
        ids: &'a [u32],
        values: &'a [f32],
        depth_ids: &'a [u32],
        depths: &'a [f32],
        lead: u32,
    ) -> Self {
        Self { ids, col_of: &[], values, depth_ids, depth_col_of: &[], depths, lead, first_sample: 0, nodes: &[] }
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
        let dcols = self.depth_ids.len();
        let dstart = i * dcols;
        let d = self.depths.get(dstart..dstart + dcols).unwrap_or(&[]);
        match self.values.get(start..start + cols) {
            Some(v) => ModPlaneRef::with_depths(self.ids, v, self.depth_ids, d),
            None => ModPlaneRef::with_depths(self.ids, &[], self.depth_ids, d),
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
        self.scalar_at_frame_opt(source_id, frame).unwrap_or(0.0)
    }

    /// [`Self::scalar_at_frame`] の、 面に無い id (r.md #115: バイパス中の source / 行の無い面)
    /// を `None` で返す版。 合成 (`modulation_offset_norm_with`) はこれで routing を飛ばす。
    #[must_use]
    #[inline]
    pub fn scalar_at_frame_opt(&self, source_id: u32, frame: u32) -> Option<f32> {
        let rows = self.rows();
        if rows == 0 {
            return None;
        }
        // 列は 1 回だけ引く (per-sample 経路。ソース数に比例する走査をしない)。
        let col = self.column(source_id)?;
        let cols = self.ids.len();
        let (a, b, t) = self.segment(frame);
        let va = *self.values.get(a.min(rows - 1) * cols + col)?;
        if b >= rows {
            return Some(va);
        }
        let vb = self.values.get(b * cols + col).copied().unwrap_or(va);
        Some(va + (vb - va) * t)
    }

    /// `frame` における `routing_id` の実効深さ (r.md #89 Q9)。深さが動かない
    /// 変調は `None` = 呼び出し側が `ModRouting::depth` を使う。
    #[must_use]
    #[inline]
    pub fn depth_at_frame(&self, routing_id: u32, frame: u32) -> Option<f32> {
        let rows = self.rows();
        if rows == 0 {
            return None;
        }
        // 列は 1 回だけ引く (routing ごと・刻みごとの経路。深さの列数に比例する走査をしない)。
        let col = self.depth_column(routing_id)?;
        let dcols = self.depth_ids.len();
        let (a, b, t) = self.segment(frame);
        let va = *self.depths.get(a.min(rows - 1) * dcols + col)?;
        if b >= rows {
            return Some(va);
        }
        let vb = self.depths.get(b * dcols + col).copied().unwrap_or(va);
        Some(va + (vb - va) * t)
    }
}
