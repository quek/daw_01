//! r.md #89: `ModRouting` の安定 id を **配る側**の経路の回帰。
//!
//! `AutomationTarget::ModRoutingDepth { routing_id }` が 1 本の変調を指すように
//! なったので、id を配る / 落とす経路が 1 つでも抜けると、参照が**別の変調**へ
//! 解決したり、何も動かさない行が保存されたりする。ここで固定するのは 3 つ:
//!
//! 1. **複製 / 貼り付けは変調に新しい id を配る。** clone したままだと
//!    `Song::mod_routing_owner` も `all_mod_routings().find` も先頭 (= 元トラック)
//!    に解決するので、複製側の深さレーンが元トラックの変調を指す。保存 → 再読込
//!    では `ensure_ids` の重複解決が複製側だけを改番するため、**その誤参照が固定
//!    される** (ファイルに焼き付いて元に戻せない)。
//! 2. **変調を落とす経路は連鎖掃除を通る。** 変調を 1 本消すと、その深さを指して
//!    いた別の変調 / レーンが dangling になる。残すとレーン一覧に効かない行が出て
//!    保存され、次に開くと `normalize_after_load` が無言で捨てる (dirty も立たない
//!    ので消えたことに気付けない)。
//! 3. **別プロジェクトからはモジュレーターを持ち込めない。** `source_id` は
//!    `Song.mod_sources` の id なので、検証せずに貼ると**たまたま同じ id の無関係な
//!    モジュレーター**に結線される。

use common::model::{
    AutomationTarget, ModRouting, ModSource, ModSourceKind, PluginInstance, Polarity, Track,
    TrackBuiltinParam,
};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::clipboard::{TrackCopy, TracksCopy};

use super::support::build_app;

const TRACK_A: u32 = 100;
const DEVICE_A: u64 = 11;
const PARAM_ID: u32 = 7;

/// トラック 1 本だけの曲にする (device 無し = 複製 / 削除が同期実行になる)。
fn single_track_app() -> AppData {
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks
            .push(Track { id: TRACK_A, name: "Lead".into(), ..Track::default() });
    });
    app
}

fn add_source(app: &mut AppData, owner_track_id: u32) -> u32 {
    app.edit_song(|song| {
        let id = song.alloc_mod_source_id();
        song.mod_sources.push(ModSource {
            id,
            owner_track_id,
            color: [0.3, 0.7, 1.0],
            kind: ModSourceKind::default(),
        });
        id
    })
    .expect("edit_song")
}

/// `target` に `source_id` を繋ぎ、できた変調の安定 id を返す。
fn connect(app: &mut AppData, target: &AutomationTarget, source_id: u32) -> u32 {
    app.handle_event(AppEvent::AddModRouting {
        track_id: TRACK_A,
        target: target.clone(),
        source_id,
    });
    app.song_doc
        .song()
        .all_mod_routings()
        .find(|r| r.source_id == source_id && &r.target == target)
        .map(|r| r.id)
        .expect("繋いだ変調が引ける")
}

/// 深さ欄を触って `A` を押す production 経路そのもの (= 深さのオートメーション
/// レーンを作る唯一の口)。
fn add_depth_lane(app: &mut AppData, target: &AutomationTarget, source_id: u32) {
    app.handle_event(AppEvent::SetModRoutingDepth {
        track_id: TRACK_A,
        target: target.clone(),
        source_id,
        depth: 0.5,
    });
    app.handle_event(AppEvent::AddAutomationFromLastTouched);
}

/// `track` の lane / routing が持つ `ModRoutingDepth` 参照を全部集める。
fn depth_refs(track: &Track) -> Vec<u32> {
    track
        .automation_lanes
        .iter()
        .map(|l| &l.target)
        .chain(track.mod_routings.iter().map(|r| &r.target))
        .filter_map(|t| match t {
            AutomationTarget::ModRoutingDepth { routing_id } => Some(*routing_id),
            _ => None,
        })
        .collect()
}

/// 複製したトラックの変調は新しい id をもらい、その深さを指す参照は
/// **複製側の変調**へ張り替わる。
#[test]
fn duplicating_a_track_renumbers_routings_and_repoints_depth_refs() {
    let mut app = single_track_app();
    let lfo_a = add_source(&mut app, TRACK_A);
    let lfo_b = add_source(&mut app, TRACK_A);

    let volume = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
    let volume_routing = connect(&mut app, &volume, lfo_a);
    // その深さ自体を別のソースで変調 + 深さのレーンも作る (クロス変調)。
    let depth = AutomationTarget::ModRoutingDepth { routing_id: volume_routing };
    connect(&mut app, &depth, lfo_b);
    add_depth_lane(&mut app, &volume, lfo_a);
    // モジュレーター自身のツマミ (LFO A の速さ ← LFO B) も 1 本繋いでおく。
    let rate = AutomationTarget::ModSourceParam {
        source_id: lfo_a,
        param: common::model::ModParam::Rate,
    };
    connect(&mut app, &rate, lfo_b);
    assert_eq!(
        depth_refs(&app.song_doc.song().tracks[0]).len(),
        2,
        "前提: 深さレーン 1 本 + 深さへの変調 1 本"
    );

    app.handle_event(AppEvent::DuplicateTracksShared(vec![TRACK_A]));

    let song = app.song_doc.song();
    assert_eq!(song.tracks.len(), 2, "複製されている");
    let ids: Vec<u32> = song.all_mod_routings().map(|r| r.id).collect();
    let uniq: std::collections::HashSet<u32> = ids.iter().copied().collect();
    assert_eq!(
        ids.len(),
        uniq.len(),
        "ModRouting.id の Song-global unique が壊れていない: {ids:?}"
    );
    assert!(!ids.contains(&0), "未採番 sentinel のまま残さない: {ids:?}");
    // モジュレーター本体は複製されないので、そのツマミへの変調は複製してはいけない。
    // `mod_graph::build_plan` は `ModSourceParam` の辺を置き場に関係なく
    // `all_mod_routings()` から集めて加算するので、2 本になると変調が 2 倍掛かる。
    let rate_edges = song
        .all_mod_routings()
        .filter(|r| matches!(r.target, AutomationTarget::ModSourceParam { .. }))
        .count();
    assert_eq!(rate_edges, 1, "モジュレーターのツマミへの変調辺が二重にならない");
    for t in &song.tracks {
        let refs = depth_refs(t);
        assert_eq!(refs.len(), 2, "複製で深さ参照が落ちていない: {refs:?}");
        for rid in refs {
            assert_eq!(
                song.mod_routing_owner(rid),
                Some(t.id),
                "深さ参照は自分のトラックの変調を指す (複製側が元トラックを指したままにならない)"
            );
        }
    }
}

/// device を消したら、その param の変調だけでなく **その深さを指していた**
/// 変調 / レーンも連鎖して消える。
#[test]
fn removing_a_device_chains_cleanup_to_depth_refs() {
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        let mut t = Track { id: TRACK_A, name: "Lead".into(), ..Track::default() };
        t.devices.push(PluginInstance {
            id: DEVICE_A,
            ..PluginInstance::with_ports(
                "test.delay".to_string(),
                PluginFormat::Clap,
                PortConfig { has_audio_input: true, has_audio_output: true, ..Default::default() },
            )
        });
        song.tracks.push(t);
    });
    let lfo_a = add_source(&mut app, TRACK_A);
    let lfo_b = add_source(&mut app, TRACK_A);

    let cutoff = AutomationTarget::PluginParam {
        device_id: DEVICE_A,
        param_id: PARAM_ID,
        legacy_device_index: None,
    };
    let cutoff_routing = connect(&mut app, &cutoff, lfo_a);
    let depth = AutomationTarget::ModRoutingDepth { routing_id: cutoff_routing };
    connect(&mut app, &depth, lfo_b);
    add_depth_lane(&mut app, &cutoff, lfo_a);
    assert_eq!(
        depth_refs(&app.song_doc.song().tracks[0]).len(),
        2,
        "前提: 深さレーン 1 本 + 深さへの変調 1 本"
    );

    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![DEVICE_A] });
    // device があるので plugin state の round-trip 待ちに積まれる。応答を fake して
    // 実行させる (production の deferred と同じ経路)。
    app.handle_event(AppEvent::Plugin(
        common::protocol::PluginEvent::AllPluginStates { entries: Vec::new() },
    ));

    let t = &app.song_doc.song().tracks[0];
    assert!(t.devices.is_empty(), "前提: device が消えている");
    assert!(
        t.mod_routings.is_empty(),
        "消えた変調の深さを変調していた行も連鎖して消える: {:?}",
        t.mod_routings
    );
    assert!(
        depth_refs(t).is_empty(),
        "何も動かさない `Mod #N depth` レーンが残らない: {:?}",
        depth_refs(t)
    );
}

fn payload_with_routing(name: &str, source_id: u32) -> TracksCopy {
    let mut track = Track { id: 900, name: name.into(), ..Track::default() };
    track.mod_routings.push(ModRouting {
        id: 1,
        target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
        source_id,
        depth: 0.5,
        polarity: Polarity::Unipolar,
    });
    TracksCopy {
        tracks: vec![TrackCopy { order: 0, track, contents: Vec::new() }],
        scenes: Vec::new(),
    }
}

fn pasted_routings(app: &AppData, name: &str) -> Vec<ModRouting> {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.mod_routings.clone())
        .expect("貼り付けたトラックが居る")
}

/// 別プロジェクトからのトラック貼り付けは、解決できない `source_id` の変調を
/// 落とす。同一プロジェクトなら残すが、id は新規採番する。
#[test]
fn pasting_across_projects_drops_modulation_it_cannot_resolve() {
    let mut app = single_track_app();
    // 貼り先が持っている、貼り元とは **無関係な** モジュレーター。
    let local = add_source(&mut app, TRACK_A);
    let pid = app.song_doc.song().project_id;

    // 別プロジェクト由来。payload の source_id が偶然 `local` と一致していても、
    // 貼り先のモジュレーターに勝手に結線してはいけない。
    app.paste_tracks_at(payload_with_routing("FromOther", local), pid ^ 0xdead_beef, TRACK_A);
    assert!(
        pasted_routings(&app, "FromOther").is_empty(),
        "別プロジェクトのモジュレーターは持ち込めない (同じ id の別物に繋がない)"
    );

    // 対照: 同一プロジェクトなら実在するので残る。ただし id は payload の値
    // (= 1) ではなく新規採番。
    app.paste_tracks_at(payload_with_routing("FromSelf", local), pid, TRACK_A);
    let kept = pasted_routings(&app, "FromSelf");
    assert_eq!(kept.len(), 1, "同一プロジェクトの変調は残る: {kept:?}");
    assert_eq!(kept[0].source_id, local);
    let ids: Vec<u32> = app.song_doc.song().all_mod_routings().map(|r| r.id).collect();
    let uniq: std::collections::HashSet<u32> = ids.iter().copied().collect();
    assert_eq!(ids.len(), uniq.len(), "貼り付けた変調にも一意な id が配られる: {ids:?}");
}
