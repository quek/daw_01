//! 組み込み内蔵 device の配置 / 番号 / 挿入位置 / ガード述語 (`docs/plan_rack_native_devices.md` §5.7)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。
//!
//! **正規化はガードの代わりにしない。** [`Song::normalize_native_devices`] が補充を行った時点で、
//! どこかの編集の口 (削除 / 切り取り / Parallel 化 / 運搬) のガードが漏れたというバグの症状になる。

use std::collections::{BTreeSet, HashMap};

use super::super::{
    AutomationLane, AutomationTarget, BindingTarget, ChainRef, Device, IdAllocators, MASTER_TRACK_ID, ModRouting,
    NativeDevice, NativeKind, Song, device_in, for_each_node_id_mut,
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

/// Song の **id 構造**の観測器。[`Song::enforce_edit_invariants`] が読むものすべてと lane id:
///
/// - トラック id (並び順込み) と、各置き場 (トラック / master) の
/// - device node (plugin / native / Parallel / chain) の id と木の形、native の種類・builtin・番号・aux 配線の有無
/// - lane の id と target、変調 routing の id・ソース・target
/// - 変調ソースの id、MIDI binding の target
///
/// 値 (params / volume / 点列 / bypass) と、clip / content / media source / section / scene の id は含まない。
/// 比べるのは前回の観測の写し全体 (ハッシュではない) なので、取り違えは起きない。
#[derive(Debug, Default)]
pub struct StructureWatch {
    /// 前回観測した構造。
    shape: Vec<ShapeItem>,
    /// 今回の観測の書き先 (確保を使い回す)。
    scratch: Vec<ShapeItem>,
    /// `shape` が不変条件を回復済みの構造か。[`Self::observe`] で観測しただけの構造は回復済みとみなさない。
    settled: bool,
}

impl StructureWatch {
    /// `song` の構造を観測する (回復済みとはみなさない = 次の [`Song::enforce_edit_invariants_watched`] は必ず回す)。
    /// undo / redo のように Song を丸ごと差し替えた後に使う。戻り値 = 前回の観測から変わったか。
    pub fn observe(&mut self, song: &Song) -> bool {
        capture_structure(song, &mut self.scratch);
        let changed = self.scratch != self.shape;
        std::mem::swap(&mut self.shape, &mut self.scratch);
        self.settled = false;
        changed
    }
}

/// [`Song::enforce_edit_invariants_watched`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforceOutcome {
    /// 不変条件の回復で Song の中身が変わったか。
    pub changed: bool,
    /// 回復後の id 構造が、前回の観測と違うか。
    pub structure_changed: bool,
}

/// [`StructureWatch`] が比べる構造の 1 要素。
#[derive(Debug, Clone, PartialEq)]
enum ShapeItem {
    /// 以降の node / lane / routing の置き場 (track id か `MASTER_TRACK_ID`)。
    Owner(u32),
    Plugin(u64),
    Native { id: u64, kind: NativeKind, builtin: bool, ordinal: u16, aux_input: bool },
    Parallel(u64),
    Chain(u64),
    ChainEnd,
    Lane { id: u32, target: AutomationTarget },
    Routing { id: u32, source_id: u32, target: AutomationTarget },
    Source(u32),
    Binding(BindingTarget),
}

fn capture_structure(song: &Song, out: &mut Vec<ShapeItem>) {
    out.clear();
    let stores = song
        .tracks
        .iter()
        .map(|t| (t.id, t.devices.as_slice(), t.automation_lanes.as_slice(), t.mod_routings.as_slice()))
        .chain(std::iter::once((
            MASTER_TRACK_ID,
            song.master_fx_chain.as_slice(),
            song.song_lanes.as_slice(),
            song.song_mod_routings.as_slice(),
        )));
    for (owner, devices, lanes, routings) in stores {
        out.push(ShapeItem::Owner(owner));
        capture_nodes(devices, out);
        out.extend(lanes.iter().map(|l: &AutomationLane| ShapeItem::Lane { id: l.id, target: l.target.clone() }));
        out.extend(routings.iter().map(|r: &ModRouting| ShapeItem::Routing {
            id: r.id,
            source_id: r.source_id,
            target: r.target.clone(),
        }));
    }
    out.extend(song.mod_sources.iter().map(|m| ShapeItem::Source(m.id)));
    out.extend(song.midi_bindings.iter().map(|b| ShapeItem::Binding(b.target)));
}

fn capture_nodes(devices: &[Device], out: &mut Vec<ShapeItem>) {
    for d in devices {
        match d {
            Device::Plugin(p) => out.push(ShapeItem::Plugin(p.id)),
            Device::Native(n) => out.push(ShapeItem::Native {
                id: n.id,
                kind: n.kind(),
                builtin: n.builtin,
                ordinal: n.ordinal,
                aux_input: n.aux_input.is_some(),
            }),
            Device::Parallel(r) => {
                out.push(ShapeItem::Parallel(r.id));
                for c in &r.chains {
                    out.push(ShapeItem::Chain(c.id));
                    capture_nodes(&c.devices, out);
                    out.push(ShapeItem::ChainEnd);
                }
            }
        }
    }
}

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

    /// 編集後の不変条件: 組み込みの正規化と dangling 参照の掃除。handler に prune を書かない。
    /// SongDoc の口は [`Self::enforce_edit_invariants_watched`] 経由で呼ぶ。
    pub fn enforce_edit_invariants(&mut self) -> bool {
        self.normalize_native_devices() | self.prune_dangling_param_targets()
    }

    /// [`Self::enforce_edit_invariants`] を、**id 構造が `watch` の前回の回復から変わったときだけ**回す。
    ///
    /// 不変条件の回復は id 構造 ([`StructureWatch`]) だけの関数で冪等なので、回復済みの構造と同じ構造なら
    /// 何も変わらない。値だけの編集 (つまみのドラッグ / plugin の再構築) で曲全体の node 表を作り直さない。
    /// 変わったかは値の比較で決まるので、呼び出し側が「構造を変えた」と宣言する必要は無い。
    pub fn enforce_edit_invariants_watched(&mut self, watch: &mut StructureWatch) -> EnforceOutcome {
        capture_structure(self, &mut watch.scratch);
        if watch.settled && watch.scratch == watch.shape {
            return EnforceOutcome { changed: false, structure_changed: false };
        }
        let changed = self.enforce_edit_invariants();
        if changed {
            capture_structure(self, &mut watch.scratch);
        }
        let structure_changed = watch.scratch != watch.shape;
        std::mem::swap(&mut watch.shape, &mut watch.scratch);
        watch.settled = true;
        EnforceOutcome { changed, structure_changed }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AuxInputRoute, CompParam, MidiBindInput, MidiBinding, ModSource, ModSourceKind, NativeParamId, Parallel,
        PluginInstance, Polarity, Track, TrackBuiltinParam,
    };
    use crate::plugin_format::PluginFormat;

    fn comp_thr(device_id: u64) -> AutomationTarget {
        AutomationTarget::NativeParam { device_id, param: NativeParamId::Comp(CompParam::Threshold) }
    }

    fn plugin(id: u64) -> Device {
        Device::Plugin(PluginInstance { id, ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap) })
    }

    fn binding(device_id: u64) -> MidiBinding {
        MidiBinding {
            channel: 0,
            input: MidiBindInput::ControlChange(1),
            legacy_controller: None,
            target: BindingTarget::NativeParam { device_id, param: NativeParamId::On(NativeKind::Comp) },
        }
    }

    /// トラック 1: plugin 5 / Parallel 10 (chain 11 に plugin 13) / 追加の Comp 14、トラック 2 と master は空。
    /// 回復で組み込みが 100.. に入った不動点を返す。
    fn settled_song() -> Song {
        let mut p = Parallel::new();
        p.id = 10;
        p.chains[0].id = 11;
        p.chains[0].devices = vec![plugin(13)];
        let track1 = Track {
            id: 1,
            devices: vec![plugin(5), Device::Parallel(p), Device::Native(NativeDevice::new_added(NativeKind::Comp, 14, 2))],
            automation_lanes: vec![
                AutomationLane::new(comp_thr(100), 0.0),
                AutomationLane::new(AutomationTarget::PluginParam { device_id: 5, param_id: 1, legacy_device_index: None }, 0.0),
                AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id: 11 }), 0.0),
            ],
            mod_routings: vec![ModRouting {
                id: 1,
                target: AutomationTarget::NativeParam { device_id: 14, param: NativeParamId::Comp(CompParam::Ratio) },
                source_id: 1,
                depth: 0.5,
                polarity: Polarity::Unipolar,
                enabled: true,
            }],
            ..Track::default()
        };
        let mut song = Song {
            tracks: vec![track1, Track { id: 2, ..Track::default() }],
            master_fx_chain: Vec::new(),
            ids: IdAllocators { next_device_id: 100, ..Song::default().ids },
            mod_sources: vec![ModSource { id: 1, owner_track_id: 1, color: [1.0; 3], kind: ModSourceKind::default(), enabled: true }],
            midi_bindings: vec![binding(100)],
            ..Song::default()
        };
        song.enforce_edit_invariants();
        assert!(!song.enforce_edit_invariants(), "前提: 不動点");
        assert_eq!(song.tracks[0].automation_lanes.len(), 3, "前提: lane は全部生きている");
        song
    }

    fn native(s: &mut Song, id: u64) -> &mut NativeDevice {
        s.native_by_id_mut(id).expect("native")
    }

    type Edit = (&'static str, fn(&mut Song));

    /// 構造を変える編集。最後の 2 つは回復では何も変わらないが構造は変わる。
    const STRUCTURAL: &[Edit] = &[
        ("組み込みを消す", |s| drop(s.remove_device(100))),
        ("builtin を外す", |s| native(s, 101).builtin = false),
        ("追加分の番号を壊す", |s| native(s, 14).ordinal = 0),
        ("SC を受けない種類に aux 配線", |s| native(s, 101).aux_input = Some(AuxInputRoute::post_fader(2))),
        ("組み込みを Parallel の中へ", |s| {
            let d = s.remove_device(101).expect("eq");
            s.chain_devices_mut(ChainRef::Chain(11)).expect("chain").push(d);
        }),
        ("dangling な lane", |s| s.tracks[1].automation_lanes.push(AutomationLane::new(comp_thr(999), 0.0))),
        ("lane の target を種類違いに", |s| s.tracks[0].automation_lanes[0].target = comp_thr(101)),
        ("lane を別トラックへ", |s| {
            let lane = s.tracks[0].automation_lanes.remove(0);
            s.tracks[1].automation_lanes.push(lane);
        }),
        ("lane が指す plugin を消す", |s| drop(s.remove_device(5))),
        ("routing のソースを消えたものに", |s| s.tracks[0].mod_routings[0].source_id = 9),
        ("変調ソースを消す", |s| s.mod_sources.clear()),
        ("dangling な binding", |s| s.midi_bindings.push(binding(999))),
        ("組み込みの無いトラックを足す", |s| s.tracks.push(Track { id: 3, ..Track::default() })),
        ("Parallel の中の plugin を最上位へ", |s| {
            let d = s.remove_device(13).expect("plugin");
            s.tracks[0].devices.insert(0, d);
        }),
        ("有効な lane を足す", |s| {
            let volume = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
            s.tracks[0].automation_lanes.push(AutomationLane::new(volume, 1.0));
        }),
    ];

    /// 値だけの編集。
    const VALUES: &[Edit] = &[
        ("native の値", |s| {
            let _ = native(s, 100).set_param(NativeParamId::Comp(CompParam::Threshold), -20.0);
        }),
        ("bypass", |s| native(s, 101).bypassed = true),
        ("トラックの音量", |s| s.tracks[0].volume = 0.5),
        ("lane の既定値", |s| s.tracks[0].automation_lanes[0].default_value = 0.3),
        ("変調の深さ", |s| s.tracks[0].mod_routings[0].depth = 0.9),
        ("plugin の state", |s| {
            if let Some(Device::Plugin(p)) = s.device_by_id_mut(5) {
                p.state = Some(std::sync::Arc::from(&[1u8, 2, 3][..]));
            }
        }),
    ];

    /// 構造を変える編集では、観測付きの回復は無条件の回復と**必ず同じ Song** になる (enforce が読む入力の
    /// 変化を 1 つも見落とさない)。値だけの編集では構造は変わらず、回復も回らない。
    #[test]
    fn watched_enforce_matches_unconditional_enforce_and_skips_value_edits() {
        let base = settled_song();
        for (name, edit) in STRUCTURAL {
            let mut watch = StructureWatch::default();
            let mut watched = base.clone();
            assert!(!watched.enforce_edit_invariants_watched(&mut watch).changed, "{name}: 前提");
            let mut plain = watched.clone();
            edit(&mut watched);
            edit(&mut plain);
            let out = watched.enforce_edit_invariants_watched(&mut watch);
            let plain_changed = plain.enforce_edit_invariants();
            assert_eq!(watched, plain, "{name}: 無条件の回復と同じ Song");
            // 構造が変わったかは回復後の最終形で比べる (壊した番号が同じ番号に直れば変わっていない)。
            let mut reference = StructureWatch::default();
            reference.observe(&base);
            let net = reference.observe(&plain);
            assert_eq!(out, EnforceOutcome { changed: plain_changed, structure_changed: net }, "{name}");
            assert!(out.changed || net, "{name}: 構造を変える編集として意味がある");
            let again = watched.enforce_edit_invariants_watched(&mut watch);
            assert_eq!(again, EnforceOutcome { changed: false, structure_changed: false }, "{name}: 2 回目");
        }
        for (name, edit) in VALUES {
            let mut watch = StructureWatch::default();
            let mut song = base.clone();
            song.enforce_edit_invariants_watched(&mut watch);
            edit(&mut song);
            let edited = song.clone();
            let out = song.enforce_edit_invariants_watched(&mut watch);
            assert_eq!(out, EnforceOutcome { changed: false, structure_changed: false }, "{name}");
            assert_eq!(song, edited, "{name}");
        }
    }

    /// `observe` しただけの構造は回復済みとみなさない: 観測後の最初の回復は、構造が同じでも必ず回す
    /// (undo で差し替えた Song が不動点でなくても直る)。
    #[test]
    fn observed_structure_is_not_trusted_as_settled() {
        let mut song = settled_song();
        let mut watch = StructureWatch::default();
        song.enforce_edit_invariants_watched(&mut watch);
        song.tracks[1].automation_lanes.push(AutomationLane::new(comp_thr(999), 0.0));
        assert!(watch.observe(&song), "dangling な lane の追加を観測する");
        let out = song.enforce_edit_invariants_watched(&mut watch);
        assert!(out.changed && out.structure_changed, "観測しただけの構造でも回復は回る: {out:?}");
        assert!(song.tracks[1].automation_lanes.is_empty());
        assert!(!watch.observe(&song), "同じ構造");
    }
}
