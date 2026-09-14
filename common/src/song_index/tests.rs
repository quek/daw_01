use super::*;
use crate::model::{
    AutomationClip, AutomationLane, AutomationTarget, ModRouting, Parallel, ParallelChain, PluginInstance, Polarity,
    SessionAutomationClip, Track, TrackBuiltinParam as B,
};
use crate::plugin_format::PluginFormat;

fn plugin(device_id: u64, param_id: u32) -> AutomationTarget {
    AutomationTarget::PluginParam { device_id, param_id, legacy_device_index: None }
}

fn routing(id: u32, target: AutomationTarget) -> ModRouting {
    ModRouting { id, target, source_id: 1, depth: 0.5, polarity: Polarity::Unipolar, enabled: true }
}

fn lane(id: u32, target: AutomationTarget, enabled: bool) -> AutomationLane {
    AutomationLane { id, enabled, ..AutomationLane::new(target, 0.0) }
}

fn plug(id: u64) -> Device {
    Device::Plugin(PluginInstance { id, ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap) })
}

fn auto_clip(id: u32, start_beat: f64, length_beats: f64) -> AutomationClip {
    AutomationClip {
        id,
        name: String::new(),
        start_beat,
        length_beats,
        content_id: 0,
        content_offset_beats: 0.0,
        color: None,
    }
}

/// 索引で引いた結果が、置き場を全件舐めて得る結果 (旧実装の規則) と同じ。
/// device の target は初出順、target の routing は置き場の並び順、lane は並び順で最初の有効なもの。
#[test]
fn 索引で引いた結果が全件走査と一致する() {
    let volume = AutomationTarget::TrackBuiltin(B::Volume);
    let routings = vec![
        routing(1, plugin(7, 2)),
        routing(2, volume.clone()),
        routing(3, plugin(9, 1)),
        routing(4, plugin(7, 1)),
        routing(5, plugin(7, 2)),
    ];
    let lanes =
        vec![lane(1, plugin(7, 1), false), lane(2, volume.clone(), true), lane(3, plugin(7, 1), true), lane(4, plugin(9, 1), true)];
    let index = ParamStoreIndex::build(&lanes, &routings);
    let store = ParamStore::new(&lanes, &routings, &index);

    let targets: Vec<(AutomationTarget, Vec<u32>)> = store
        .targets_of(ParamSubject::Plugin(7))
        .map(|(t, r)| (t.clone(), r.iter().map(|r| r.id).collect()))
        .collect();
    assert_eq!(targets, vec![(plugin(7, 2), vec![1, 5]), (plugin(7, 1), vec![4])]);
    assert_eq!(store.routings_for(&volume).iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
    assert!(store.routings_for(&plugin(8, 1)).is_empty());

    let lanes_of_7: Vec<usize> = store.lanes_of(ParamSubject::Plugin(7)).map(|v| v.pos).collect();
    assert_eq!(lanes_of_7, vec![0, 2]);
    assert_eq!(store.enabled_lane(&plugin(7, 1)).map(|v| v.pos), Some(2), "無効な lane は飛ばす");
    assert_eq!(store.enabled_lane(&plugin(7, 2)).map(|v| v.pos), None);
    assert_eq!(store.lane_by_id(3).map(|v| v.pos), Some(2));
    assert!(store.lane_by_id(99).is_none());
}

/// lane のアレンジ clip は `lane_value_at` と同じ clip を、セルは clip id で引く。
#[test]
fn lane_の_clip_とセルを線形探索と同じに引く() {
    let mut l = lane(1, AutomationTarget::TrackBuiltin(B::Pan), true);
    // 並びは開始拍順ではない (分割の右断片は末尾に積まれる)。
    l.clips = vec![auto_clip(1, 8.0, 4.0), auto_clip(2, 0.0, 4.0), auto_clip(3, 4.0, 0.0), auto_clip(4, 4.0, 4.0)];
    l.session_clips = vec![
        SessionAutomationClip { scene_id: 1, clip: auto_clip(7, 0.0, 1.0), launch: Default::default() },
        SessionAutomationClip { scene_id: 2, clip: auto_clip(5, 0.0, 2.0), launch: Default::default() },
    ];
    let lanes = vec![l];
    let index = ParamStoreIndex::build(&lanes, &[]);
    let view = ParamStore::new(&lanes, &[], &index).lane(0).unwrap();
    let linear = |x: f64| {
        lanes[0].clips.iter().find(|c| c.length_beats > 0.0 && x >= c.start_beat && x < c.start_beat + c.length_beats).map(|c| c.id)
    };
    for x in [-1.0, 0.0, 3.9, 4.0, 7.99, 8.0, 12.0, 20.0] {
        assert_eq!(view.arrangement_clip(x).map(|c| c.id), linear(x), "x={x}");
    }
    assert_eq!(view.cell(5).map(|c| c.scene_id), Some(2));
    assert!(view.cell(6).is_none());
}

/// 別の snapshot の索引 (本数が合わない) は空として扱う (外れた位置を引かない)。
#[test]
fn 本数の合わない索引は空として扱う() {
    let routings = vec![routing(1, plugin(7, 1))];
    let index = ParamStoreIndex::build(&[], &routings);
    let fewer: Vec<ModRouting> = Vec::new();
    let store = ParamStore::new(&[], &fewer, &index);
    assert_eq!(store.targets_of(ParamSubject::Plugin(7)).count(), 0);
}

/// device / chain / track / scene / send / セルは `Song` の線形探索と同じものを引く (入れ子の Parallel と master 込み)。
/// 同じ id の device / chain が別の置き場にある (壊れた曲) ときも、線形探索が先に見つける方を引く。
#[test]
fn device_chain_track_の引き当てが_song_の探索と一致する() {
    let native = |id: u64| Device::Native(crate::model::NativeDevice::new_added(crate::model::NativeKind::Comp, id, 1));
    let mut inner = Parallel::new();
    inner.id = 20;
    inner.chains = vec![ParallelChain { id: 11, devices: vec![plug(22), native(51)], ..ParallelChain::new("in") }];
    let mut outer = Parallel::new();
    outer.id = 10;
    outer.chains = vec![
        ParallelChain { id: 11, devices: vec![plug(12), native(50)], ..ParallelChain::new("a") },
        ParallelChain { id: 13, devices: vec![Device::Parallel(inner), plug(14)], ..ParallelChain::new("b") },
    ];
    let mut t1 = Track { id: 5, devices: vec![plug(1), Device::Parallel(outer)], ..Track::default() };
    t1.sends = vec![
        crate::model::Send { id: 3, dest_track_id: 6, gain: 0.5, mode: crate::model::SendMode::PostFader, enabled: true },
        crate::model::Send { id: 2, dest_track_id: 6, gain: 0.25, mode: crate::model::SendMode::PostFader, enabled: true },
    ];
    let t2 = Track { id: 6, devices: vec![plug(51), native(51), plug(30)], ..Track::default() };
    let song = Song {
        tracks: vec![t1, t2],
        master_fx_chain: vec![plug(40), native(50)],
        scenes: vec![crate::model::Scene::new(9), crate::model::Scene::new(4)],
        ..Song::default()
    };
    let index = SongIndex::build(&song);
    for id in [1, 10, 12, 14, 20, 22, 30, 40, 11, 50, 51, 99] {
        assert_eq!(index.device(&song, id).map(Device::id), song.device_by_id(id).map(Device::id), "device {id}");
    }
    for (parallel_id, chain_id) in [(10, 11), (10, 13), (20, 11), (20, 13), (12, 11), (99, 11)] {
        let got = index.chain_in(&song, parallel_id, chain_id).map(std::ptr::from_ref);
        let want = song.parallel_by_id(parallel_id).and_then(|p| p.chains.iter().find(|c| c.id == chain_id)).map(std::ptr::from_ref);
        assert_eq!(got, want, "chain {chain_id} in {parallel_id}");
    }
    let owners = [ParamStoreAt::Track(0), ParamStoreAt::Track(1), ParamStoreAt::Song];
    for (owner, devices) in owners.into_iter().zip([&song.tracks[0].devices, &song.tracks[1].devices, &song.master_fx_chain]) {
        for id in [1, 10, 14, 22, 30, 40, 50, 51] {
            let got = index.native_in(&song, owner, id).map(std::ptr::from_ref);
            assert_eq!(got, crate::model::native_in(devices, id).map(std::ptr::from_ref), "native {id} in {owner:?}");
        }
    }
    assert_eq!(index.parallel(&song, 20).map(|p| p.id), Some(20));
    assert!(index.parallel(&song, 22).is_none(), "plugin は Parallel ではない");
    assert_eq!((index.track_pos(6), index.track_pos(7)), (Some(1), None));
    assert_eq!((index.scene_pos(4), index.scene_pos(5)), (Some(1), None));
    assert_eq!(index.send(&song, 0, 2).map(|s| s.gain), Some(0.25));
    assert!(index.send(&song, 1, 2).is_none());
}

fn session_clip(id: u32, scene_id: u32, length_beats: f64) -> crate::model::SessionClip {
    crate::model::SessionClip {
        scene_id,
        clip: crate::model::Clip { id, length_beats, ..crate::model::Clip::default() },
        launch: Default::default(),
    }
}

/// ランチャーの行はトラック行 → そのレーン行 → マスター行の順 (テンポ / 拍子レーンは外す)、セルは clip id / 列で
/// 並びの先頭のものを、列の最長は行ごとの先頭のセルの長さの最大を引く。
#[test]
fn ランチャーの行とセルを線形探索と同じに引く() {
    let mut t1 = Track { id: 5, ..Track::default() };
    // 列 2 にセルが 2 つ (先頭の 3 拍が勝つ)。
    t1.session_clips = vec![session_clip(10, 2, 3.0), session_clip(11, 1, 8.0), session_clip(12, 2, 16.0)];
    let mut pan = lane(4, AutomationTarget::TrackBuiltin(B::Pan), true);
    pan.session_clips = vec![SessionAutomationClip { scene_id: 2, clip: auto_clip(7, 0.0, 6.0), launch: Default::default() }];
    t1.automation_lanes = vec![pan];
    let t2 = Track { id: 6, ..Track::default() };
    let mut song = Song { tracks: vec![t1, t2], ..Song::default() };
    let mut master = lane(9, AutomationTarget::MasterLimiter(crate::model::MasterLimiterParam::Ceiling), true);
    master.session_clips = vec![SessionAutomationClip { scene_id: 3, clip: auto_clip(1, 0.0, 2.0), launch: Default::default() }];
    song.song_lanes = vec![lane(8, AutomationTarget::SongTempo, true), master];
    let index = SongIndex::build(&song);

    let keys: Vec<(u32, u32)> = (0..index.launcher_row_count()).filter_map(|i| index.launcher_row_key(i)).collect();
    assert_eq!(keys, vec![(5, 0), (5, 4), (6, 0), (crate::model::MASTER_TRACK_ID, 9)]);
    assert_eq!(index.launcher_row_pos(6, 0), Some(2));
    assert!(index.launcher_row_pos(crate::model::MASTER_TRACK_ID, 8).is_none(), "テンポレーンは行にならない");

    let row = index.launcher_row(&song, 0).unwrap();
    assert_eq!((row.cells.clip_pos(12), row.cells.scene_pos(2), row.cells.scene_pos(9)), (Some(2), Some(0), None));
    assert_eq!(index.scene_longest(2), Some(6.0), "行ごとに列の先頭のセルだけを数える");
    assert_eq!((index.scene_longest(1), index.scene_longest(3), index.scene_longest(4)), (Some(8.0), Some(2.0), None));
}
