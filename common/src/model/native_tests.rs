//! r.md #129 F: 内蔵 device の model テスト (`docs/plan_rack_native_devices.md` §15.2 F-C1〜F-C6 / F-C8)。

use super::*;
use crate::plugin_format::PluginFormat;

fn plug(id: u64) -> Device {
    Device::Plugin(PluginInstance { id, ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap) })
}

fn native(kind: NativeKind, id: u64, builtin: bool, ordinal: u16) -> NativeDevice {
    NativeDevice { builtin, ordinal, ..NativeDevice::new_added(kind, id, ordinal) }
}

fn nd(kind: NativeKind, id: u64, builtin: bool, ordinal: u16) -> Device {
    Device::Native(native(kind, id, builtin, ordinal))
}

fn parallel(id: u64, chain_id: u64, devices: Vec<Device>) -> Device {
    let mut p = Parallel::new();
    p.id = id;
    p.chains[0].id = chain_id;
    p.chains[0].devices = devices;
    Device::Parallel(p)
}

fn song_with(tracks: Vec<(u32, Vec<Device>)>, master: Vec<Device>) -> Song {
    Song {
        tracks: tracks.into_iter().map(|(id, devices)| Track { id, devices, ..Track::default() }).collect(),
        master_fx_chain: master,
        ids: IdAllocators { next_device_id: 100, next_track_id: 10, ..Song::default().ids },
        ..Song::default()
    }
}

fn natives_of(devices: &[Device]) -> Vec<(u64, NativeKind, bool, u16)> {
    let mut out = Vec::new();
    for_each_native(devices, &mut |n| out.push((n.id, n.kind(), n.builtin, n.ordinal)));
    out
}

/// F-C1: `{"Native":{..}}` は Native、旧 plugin 配列 (untagged fallback) は Plugin として読める。
#[test]
fn native_tag_and_untagged_plugin_fallback_both_deserialize() {
    let json = r#"[{"Native":{"id":5,"builtin":true,"ordinal":1,"bypassed":true,"params":{"Eq":{}}}},
                   {"plugin_id":"x","format":"Clap"}]"#;
    let v: Vec<Device> = serde_json::from_str(json).unwrap();
    assert!(matches!(&v[0], Device::Native(n) if n.id == 5 && n.builtin && n.kind() == NativeKind::Eq && n.bypassed));
    assert!(matches!(&v[0], Device::Native(n) if n.params == NativeParams::default_of(NativeKind::Eq)));
    assert!(matches!(&v[1], Device::Plugin(p) if p.plugin_id == "x"));
    let back: Vec<Device> = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(back, v);
    let cfg = bincode::config::standard();
    let (decoded, _): (Vec<Device>, usize) =
        bincode::decode_from_slice(&bincode::encode_to_vec(&v, cfg).unwrap(), cfg).unwrap();
    assert_eq!(decoded, v);
}

/// F-C2: 正規化 — Parallel 内 / 役割違い / 同種 2 個目の builtin を降格、欠けを補充、EQ の aux を落とし、
/// 番号を修復する。2 回目は変化しない。
#[test]
fn normalize_native_devices_repairs_structure_and_is_idempotent() {
    use NativeKind::{BusComp, Comp, Eq, ToneEq};
    let mut eq_with_aux = native(Eq, 16, false, 2);
    eq_with_aux.aux_input = Some(AuxInputRoute { tap: AudioTap::new(TapSource::Track(2), TapPoint::PostFx) });
    let track = vec![
        parallel(10, 11, vec![nd(Comp, 12, true, 1)]),
        nd(ToneEq, 13, true, 1),
        nd(Comp, 14, true, 1),
        nd(Comp, 15, true, 1),
        Device::Native(eq_with_aux),
        nd(Comp, 17, false, 1),
    ];
    let mut song = song_with(vec![(1, track), (2, vec![])], vec![plug(20)]);
    assert!(song.normalize_native_devices());
    let t1 = natives_of(&song.tracks[0].devices);
    assert_eq!(
        t1,
        vec![
            (12, Comp, false, 2),  // Parallel 内の builtin は降格、builtin の 1 と衝突するので 2
            (13, ToneEq, false, 1), // 役割違いは降格 (同種が他に無いので 1 のまま)
            (14, Comp, true, 1),
            (15, Comp, false, 3),  // 同種 2 個目の builtin は降格
            (16, Eq, false, 2),
            (17, Comp, false, 4),  // 追加分の 1 は同種 builtin の 1 と衝突
            (100, Eq, true, 1),    // 欠けた Eq を末尾に補う
        ]
    );
    assert!(song.tracks[0].devices.iter().any(|d| matches!(d, Device::Native(n) if n.id == 16 && n.aux_input.is_none())));
    assert_eq!(natives_of(&song.tracks[1].devices), vec![(101, Comp, true, 1), (102, Eq, true, 1)]);
    let master: Vec<u64> = song.master_fx_chain.iter().map(Device::id).collect();
    assert_eq!(master, vec![103, 104, 20], "master は先頭に Bus Comp → Tone EQ");
    assert_eq!(natives_of(&song.master_fx_chain), vec![(103, BusComp, true, 1), (104, ToneEq, true, 1)]);
    assert!(song.tracks[1].devices.iter().all(|d| matches!(d, Device::Native(n) if n.bypassed)), "補充は bypass");
    let snapshot = song.clone();
    assert!(!song.normalize_native_devices(), "2 回目は変化しない");
    assert_eq!(song, snapshot);
}

/// F-C3: 新規 device の既定の挿入位置 (Q6)。
#[test]
fn default_insert_index_follows_q6() {
    use NativeKind::{BusComp, Comp, Eq, ToneEq};
    let synth = plug(1);
    let rev = plug(2);
    let cases: [(Vec<Device>, bool, usize); 6] = [
        (vec![synth.clone(), nd(Comp, 3, true, 1), nd(Eq, 4, true, 1)], false, 1),
        (vec![nd(Comp, 3, true, 1), synth, nd(Eq, 4, true, 1)], false, 2),
        (vec![nd(Comp, 3, true, 1), nd(Eq, 4, true, 1)], false, 0),
        (vec![nd(Comp, 3, true, 1), nd(Eq, 4, true, 1), rev.clone()], false, 3),
        (vec![nd(BusComp, 5, true, 1), nd(ToneEq, 6, true, 1)], true, 2),
        (vec![nd(BusComp, 5, true, 1), nd(ToneEq, 6, true, 1), rev], true, 3),
    ];
    for (i, (devices, is_master, want)) in cases.iter().enumerate() {
        assert_eq!(default_insert_index_in(devices, *is_master), *want, "case {i}");
    }
    let song = song_with(vec![(1, vec![parallel(10, 11, vec![plug(7), plug(8)]), nd(Comp, 3, true, 1)])], vec![]);
    assert_eq!(song.default_insert_index(ChainRef::Chain(11)), Some(2), "Parallel の中は末尾");
    assert_eq!(song.default_insert_index(ChainRef::Track(1)), Some(1));
    assert_eq!(song.default_insert_index(ChainRef::Chain(999)), None);
}

/// F-C4: 番号 (Q10 / K9)。
#[test]
fn native_ordinals_pick_the_smallest_free_number_from_two() {
    use NativeKind::{BusComp, Comp};
    let mut song = song_with(
        vec![(1, vec![nd(BusComp, 3, false, 2), nd(Comp, 4, true, 1)]), (2, vec![nd(Comp, 5, true, 1), nd(Comp, 6, false, 2)])],
        vec![],
    );
    assert_eq!(song.next_native_ordinal(1, BusComp), 3, "Bus Comp 2 だけ → 3");
    assert_eq!(song.next_native_ordinal(2, BusComp), 1, "空 → 1 (番号なし)");

    let mut copies = vec![nd(Comp, 4, true, 1), nd(Comp, 4, true, 1)];
    song.prepare_device_copies(1, &mut copies);
    let got = natives_of(&copies);
    assert_eq!(got.iter().map(|n| (n.2, n.3)).collect::<Vec<_>>(), vec![(false, 2), (false, 3)], "一括コピー 2 個 → 2, 3");
    assert!(got[0].0 != 4 && got[1].0 != 4 && got[0].0 != got[1].0, "新しい id: {got:?}");

    // トラックを跨ぐ移動: 空いていれば保つ、衝突すれば空き番号。
    let mut moved = vec![nd(Comp, 30, false, 1), nd(Comp, 31, false, 3)];
    song.assign_native_ordinals(2, &mut moved, OrdinalPolicy::KeepIfFree);
    assert_eq!(natives_of(&moved).iter().map(|n| n.3).collect::<Vec<_>>(), vec![3, 4]);
    let mut moved = vec![nd(Comp, 32, false, 5)];
    song.assign_native_ordinals(2, &mut moved, OrdinalPolicy::KeepIfFree);
    assert_eq!(natives_of(&moved)[0].3, 5, "空いている番号は保つ");
}

/// F-C5: 運搬のガード述語 (Q5)。
#[test]
fn can_relocate_keeps_builtins_on_their_own_top_level() {
    let song = song_with(
        vec![(1, vec![parallel(10, 11, vec![parallel(12, 13, vec![])]), nd(NativeKind::Comp, 3, true, 1)]), (2, vec![])],
        vec![],
    );
    assert!(!song.can_relocate(3, ChainRef::Track(2), false), "組み込みを他トラックへ move");
    assert!(!song.can_relocate(3, ChainRef::Chain(11), false), "組み込みを Parallel の中へ move");
    assert!(song.can_relocate(3, ChainRef::Track(1), false), "同じトラックの最上位での並べ替え");
    assert!(song.can_relocate(3, ChainRef::Track(2), true) && song.can_relocate(3, ChainRef::Chain(11), true), "copy はどこでも");
    assert!(!song.can_relocate(10, ChainRef::Chain(13), true), "Parallel を自分の中の chain へは copy でも不可");
    assert!(song.can_relocate(10, ChainRef::Track(2), false));
}

/// F-C6: 値の読み書きと sanitize。
#[test]
fn native_params_address_every_field_independently_and_sanitize_per_field() {
    for kind in NativeKind::ALL {
        for &p in NativeParamId::all_of(kind).iter().filter(|p| !matches!(p, NativeParamId::On(_))) {
            let mut params = NativeParams::default_of(kind);
            let before: Vec<Option<f32>> = NativeParamId::all_of(kind).iter().map(|q| params.get(*q)).collect();
            let v = p.range().clamp(p.range().from_norm(0.93) as f32);
            assert_ne!(params.get(p), Some(v), "{p:?}: 既定と同じ値では検査にならない");
            assert!(params.set(p, v), "{p:?}");
            assert_eq!(params.get(p), Some(v), "{p:?}");
            for (q, b) in NativeParamId::all_of(kind).iter().zip(&before) {
                if *q != p {
                    assert_eq!(params.get(*q), *b, "{p:?} を書いたら {q:?} が変わった");
                }
            }
        }
    }
    let mut bus = NativeParams::default_of(NativeKind::BusComp);
    assert!(bus.set(NativeParamId::BusComp(BusCompParam::Ratio), 1.4));
    assert!(matches!(bus, NativeParams::BusComp(s) if s.ratio == BusCompRatio::R4));
    assert_eq!(bus.get(NativeParamId::BusComp(BusCompParam::Ratio)), Some(1.0));
    assert!(bus.set(NativeParamId::BusComp(BusCompParam::Release), 4.0));
    assert!(matches!(bus, NativeParams::BusComp(s) if s.release == BusCompRelease::Auto));

    let mut eq = NativeParams::default_of(NativeKind::Eq);
    let untouched = eq;
    assert!(!eq.set(NativeParamId::Comp(CompParam::Threshold), -10.0), "種類違い");
    assert!(!eq.set(NativeParamId::Eq { band: EqBand::Hp, param: EqParam::Gain }, 3.0), "実在しない組");
    assert!(!eq.set(NativeParamId::Eq { band: EqBand::Lmf, param: EqParam::Gain }, f32::NAN), "非有限");
    assert!(!eq.set(NativeParamId::On(NativeKind::Eq), 0.0), "On は params に書かない");
    assert_eq!(eq, untouched);

    let mut s = EqSettings::default();
    s.lf.q = f32::NAN;
    s.lf.bell = true;
    s.hp.gain_db = f32::INFINITY;
    s.lmf.freq_hz = 99_999.0;
    let mut dirty = NativeParams::Eq(s);
    dirty.sanitize();
    let NativeParams::Eq(clean) = dirty else { unreachable!() };
    for band in EqBand::ALL {
        for p in EqParam::ALL {
            let v = clean.param(band, p);
            let (lo, hi) = p.range(band).display_range();
            assert!(v.is_finite() && f64::from(v) >= lo - 1e-6 && f64::from(v) <= hi + 1e-6, "{band:?} {p:?} = {v}");
        }
    }
    let again = dirty;
    dirty.sanitize();
    assert_eq!(dirty, again, "2 回目は不変");
}

/// F-C8: `can_activate` — 静的 bypass でも enabled な On レーン / 変調があれば処理しうる。
#[test]
fn can_activate_counts_enabled_on_lanes_and_routings() {
    let mut dev = native(NativeKind::Comp, 7, true, 1);
    dev.bypassed = true;
    let on = AutomationTarget::NativeParam { device_id: 7, param: NativeParamId::On(NativeKind::Comp) };
    assert!(!dev.can_activate(&[], &[]));
    let lane = AutomationLane::new(on.clone(), 1.0);
    assert!(dev.can_activate(std::slice::from_ref(&lane), &[]));
    let disabled = AutomationLane { enabled: false, ..lane };
    assert!(!dev.can_activate(&[disabled], &[]));
    let routing = ModRouting { id: 1, target: on, source_id: 1, depth: 0.5, polarity: Polarity::Unipolar, enabled: true };
    assert!(dev.can_activate(&[], &[routing]));
    dev.bypassed = false;
    assert!(dev.can_activate(&[], &[]));
}
