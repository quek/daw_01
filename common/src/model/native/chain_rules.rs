//! 組み込み内蔵 device の配置 / 番号 / 挿入位置 / ガード述語 (`docs/plan_rack_native_devices.md` §5.7)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。
//!
//! **正規化はガードの代わりにしない。** [`Song::normalize_native_devices`] が補充を行った時点で、
//! どこかの編集の口 (削除 / 切り取り / Parallel 化 / 運搬) のガードが漏れたというバグの症状になる。

use std::collections::{BTreeSet, HashMap};

use super::super::{
    ChainRef, Device, IdAllocators, MASTER_TRACK_ID, NativeDevice, NativeKind, Song, device_in,
    for_each_node_id_mut,
};

/// [`Song::assign_native_ordinals`] の番号の決め方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinalPolicy {
    /// 常に空き番号を新しく振る (コピー / 貼り付け)。
    Fresh,
    /// 今の番号が 1 以上で空いていれば保つ、それ以外は空き番号 (トラックを跨ぐ移動)。
    KeepIfFree,
}

/// 種類ごとの使用中の番号。
type Taken = HashMap<NativeKind, BTreeSet<u16>>;

/// 空き番号: 使用中が無ければ 1 (番号なし)、それ以外は 2 以上の最小の空き。
fn next_ordinal(taken: &BTreeSet<u16>) -> u16 {
    if taken.is_empty() {
        return 1;
    }
    (2..=u16::MAX).find(|n| !taken.contains(n)).unwrap_or(u16::MAX)
}

/// `devices` 以下 (Parallel の中を含む) の native を pre-order で可変訪問する。
fn walk_natives_mut(devices: &mut [Device], depth: usize, f: &mut impl FnMut(&mut NativeDevice, usize)) {
    for d in devices {
        match d {
            Device::Native(n) => f(n, depth),
            Device::Parallel(r) => {
                for c in &mut r.chains {
                    walk_natives_mut(&mut c.devices, depth + 1, f);
                }
            }
            Device::Plugin(_) => {}
        }
    }
}

/// `devices` 以下の native の番号を種類ごとに集める。
fn collect_taken(devices: &[Device], taken: &mut Taken) {
    super::super::for_each_native(devices, &mut |n| {
        if n.ordinal >= 1 {
            taken.entry(n.kind()).or_default().insert(n.ordinal);
        }
    });
}

/// 新規 device を置く位置 (Q6): 最上位で最後にある「組み込み以外」の直後。組み込み以外が
/// 無ければ、通常トラックは最初の組み込みの位置 (空なら 0)、master は最後の組み込みの直後。
#[must_use]
pub fn default_insert_index_in(devices: &[Device], is_master: bool) -> usize {
    let is_builtin = |d: &Device| matches!(d, Device::Native(n) if n.builtin);
    if let Some(i) = devices.iter().rposition(|d| !is_builtin(d)) {
        return i + 1;
    }
    if is_master {
        devices.iter().rposition(is_builtin).map_or(0, |i| i + 1)
    } else {
        devices.iter().position(is_builtin).unwrap_or(0)
    }
}

/// 1 本の最上位チェーンを正規化する (手順 1〜5)。戻り値 = 変化したか。
fn normalize_chain(devices: &mut Vec<Device>, is_master: bool, ids: &mut IdAllocators) -> bool {
    let roles: &[NativeKind] = if is_master { &NativeKind::BUILTIN_MASTER } else { &NativeKind::BUILTIN_TRACK };
    let mut changed = false;
    // 1. Parallel の中の builtin を降格 / 4. SC を受けない種類の aux を落とす。
    walk_natives_mut(devices, 0, &mut |n, depth| {
        if depth > 0 && n.builtin {
            n.builtin = false;
            changed = true;
        }
        if !n.kind().accepts_sidechain() && n.aux_input.take().is_some() {
            changed = true;
        }
    });
    // 2. 最上位の役割違い / 同種 2 個目以降の builtin を降格。
    let mut seen: Vec<NativeKind> = Vec::new();
    for d in devices.iter_mut() {
        if let Device::Native(n) = d
            && n.builtin
        {
            if roles.contains(&n.kind()) && !seen.contains(&n.kind()) {
                seen.push(n.kind());
            } else {
                n.builtin = false;
                changed = true;
            }
        }
    }
    // 3. 欠けた種類を補う (通常は末尾に Comp → EQ、master は先頭に Bus Comp → Tone EQ)。
    let mut front = 0;
    for &kind in roles {
        if seen.contains(&kind) {
            continue;
        }
        let dev = Device::Native(NativeDevice::new_builtin(kind, ids.alloc_device_id()));
        if is_master {
            devices.insert(front, dev);
            front += 1;
        } else {
            devices.push(dev);
        }
        changed = true;
    }
    changed | repair_ordinals(devices)
}

/// 手順 5: builtin は 1、それ以外は有効な番号 (1 以上・重複なし・同種 builtin の 1 と衝突しない) を
/// 保ち、無効なものに空き番号を pre-order で振る。
fn repair_ordinals(devices: &mut [Device]) -> bool {
    let mut changed = false;
    let mut taken: Taken = HashMap::new();
    let mut invalid = 0usize;
    // builtin は最上位にしか居ない (手順 1 の後)。先に 1 を予約する。
    for d in devices.iter_mut() {
        if let Device::Native(n) = d
            && n.builtin
        {
            if n.ordinal != 1 {
                n.ordinal = 1;
                changed = true;
            }
            taken.entry(n.kind()).or_default().insert(1);
        }
    }
    walk_natives_mut(devices, 0, &mut |n, _| {
        if n.builtin {
            return;
        }
        let set = taken.entry(n.kind()).or_default();
        if n.ordinal >= 1 && set.insert(n.ordinal) {
            return;
        }
        // 無効: 後で振る印として 0 に落とす (同じ走査で振ると後続の有効番号と衝突しうる)。
        if n.ordinal != 0 {
            changed = true;
        }
        n.ordinal = 0;
        invalid += 1;
    });
    if invalid > 0 {
        walk_natives_mut(devices, 0, &mut |n, _| {
            if !n.builtin && n.ordinal == 0 {
                let set = taken.entry(n.kind()).or_default();
                n.ordinal = next_ordinal(set);
                set.insert(n.ordinal);
                changed = true;
            }
        });
    }
    changed
}

impl Song {
    /// 組み込みの構造不変条件を回復する (Parallel 内の builtin 降格 / 役割違い・重複の降格 /
    /// 欠けの補充 / SC を受けない種類の aux 除去 / 番号の修復)。**冪等**。
    pub fn normalize_native_devices(&mut self) -> bool {
        let Song { tracks, master_fx_chain, ids, .. } = self;
        let mut changed = false;
        for t in tracks.iter_mut() {
            changed |= normalize_chain(&mut t.devices, false, ids);
        }
        changed | normalize_chain(master_fx_chain, true, ids)
    }

    /// 編集後の不変条件: 組み込みの正規化と dangling 参照の掃除。SongDoc の口 (new / edit /
    /// normalize / normalize_checked / replace_song) が無条件に呼ぶ。handler に prune を書かない。
    pub fn enforce_edit_invariants(&mut self) -> bool {
        self.normalize_native_devices() | self.prune_dangling_param_targets()
    }

    /// `dest` へ新規 device を挿す既定位置。`None` = `dest` が無い。Parallel の中は末尾。
    #[must_use]
    pub fn default_insert_index(&self, dest: ChainRef) -> Option<usize> {
        let devices = self.chain_devices(dest)?;
        Some(match dest {
            ChainRef::Track(tid) => default_insert_index_in(devices, tid == MASTER_TRACK_ID),
            ChainRef::Chain(_) => devices.len(),
        })
    }

    /// `owner` (track id か `MASTER_TRACK_ID`) に `kind` を 1 個足すときの番号。
    #[must_use]
    pub fn next_native_ordinal(&self, owner: u32, kind: NativeKind) -> u16 {
        let mut taken = Taken::new();
        if let Some(devices) = self.fx_chain_by_track_id(owner) {
            collect_taken(devices, &mut taken);
        }
        next_ordinal(taken.entry(kind).or_default())
    }

    /// `devices` (まだ `dest_owner` に居ない一括) の native に番号を振る。使用中 = dest の木全体 +
    /// この一括で振った分。builtin は触らない。
    pub fn assign_native_ordinals(&self, dest_owner: u32, devices: &mut [Device], policy: OrdinalPolicy) {
        let mut taken = Taken::new();
        if let Some(dest) = self.fx_chain_by_track_id(dest_owner) {
            collect_taken(dest, &mut taken);
        }
        walk_natives_mut(devices, 0, &mut |n, _| {
            if n.builtin {
                return;
            }
            let set = taken.entry(n.kind()).or_default();
            let keep = policy == OrdinalPolicy::KeepIfFree && n.ordinal >= 1 && !set.contains(&n.ordinal);
            if !keep {
                n.ordinal = next_ordinal(set);
            }
            set.insert(n.ordinal);
        });
    }

    /// コピー / 貼り付けする一括を `dest_owner` へ入れる準備: 全ノードに新しい id、native は
    /// 追加分 (`builtin = false`) にして値を sanitize、番号を `Fresh` で振る。splice の前に 1 回だけ呼ぶ。
    pub fn prepare_device_copies(&mut self, dest_owner: u32, devices: &mut [Device]) {
        for_each_node_id_mut(devices, &mut |id| *id = self.ids.alloc_device_id());
        walk_natives_mut(devices, 0, &mut |n, _| {
            n.builtin = false;
            n.ordinal = 0;
            n.sanitize();
        });
        self.assign_native_ordinals(dest_owner, devices, OrdinalPolicy::Fresh);
    }

    /// `id` が組み込み native か。
    #[must_use]
    pub fn is_builtin_native(&self, id: u64) -> bool {
        matches!(self.device_by_id(id), Some(Device::Native(n)) if n.builtin)
    }

    /// `id` の device を `dest` へ運べるか。Parallel を自分の中の chain へ落とすのは copy でも不可。
    /// 組み込みは copy でなければ所属トラックの最上位の中でしか動かせない (Q5)。
    #[must_use]
    pub fn can_relocate(&self, id: u64, dest: ChainRef, copy: bool) -> bool {
        if self.dest_inside_device(id, dest) {
            return false;
        }
        if copy || !self.is_builtin_native(id) {
            return true;
        }
        self.device_owner_track(id).is_some_and(|owner| dest == ChainRef::Track(owner))
    }

    /// `dest` が `device_id` (Parallel) の**中**の chain か (自分の中へ落とす循環)。
    fn dest_inside_device(&self, device_id: u64, dest: ChainRef) -> bool {
        let ChainRef::Chain(cid) = dest else {
            return false;
        };
        let Some(owner) = self.device_owner_track(device_id) else {
            return false;
        };
        self.fx_chain_by_track_id(owner)
            .and_then(|devices| device_in(devices, device_id))
            .and_then(Device::as_parallel)
            .is_some_and(|r| {
                r.chains
                    .iter()
                    .any(|c| c.id == cid || super::super::chain_devices_in(&c.devices, cid).is_some())
            })
    }
}
