//! v39 (r.md #129): 旧 `Track.strip` / `Song.master_strip` を組み込み native device へ移す JSON 前処理
//! (`docs/plan_rack_native_devices.md` §6.2 / §6.3)。
//!
//! [`migrate_strips_to_native`] は `migrate_legacy_song` の末尾から**版に依存せず**呼ばれる
//! (ファイル load / script の `appLoadSongJson` / `loadSongFromObject` / export_bench が通る唯一の口)。
//! 旧形 (`strip` / `master_strip` / `TrackBuiltin(Strip*)` / `MasterStrip(..)`) と新形は重ならないので冪等。
//!
//! オートメーション / 変調の target は **実 id** の `NativeParam` に書き換える (sentinel を作らない)。
//! 実在しない EQ の組 (ノブが無く GUI から作れない組) と、置き場違いの strip target は削除する
//! (残すと unknown variant で deserialize が落ちる)。旧型は Rust の型として残さない。

use serde_json::{Map, Value, json};

use crate::model::NativeParamId;

/// target の書き換え結果。
enum Rewrite {
    Keep,
    Replace(Value),
    Drop,
}

/// song JSON 全体の strip を組み込み native へ移す。旧形が 1 つも無ければ何もしない。
pub(crate) fn migrate_strips_to_native(song: &mut Value) {
    if !has_legacy_strips(song) {
        return;
    }
    let Some(obj) = song.as_object_mut() else { return };
    let recorded = obj.get("ids").and_then(|ids| ids.get("next_device_id")).and_then(Value::as_u64);
    let mut next = recorded.unwrap_or(0).max(max_node_id(obj).saturating_add(1)).max(1);
    if let Some(tracks) = obj.get_mut("tracks").and_then(Value::as_array_mut) {
        for track in tracks {
            migrate_strips_in_track_value(track, &mut next);
        }
    }
    migrate_master_strip(obj, &mut next);
    let ids = obj.entry("ids").or_insert_with(|| Value::Object(Map::new()));
    if let Some(ids) = ids.as_object_mut() {
        ids.insert("next_device_id".to_string(), Value::from(next));
    }
}

/// トラック 1 本分 (`devices` の組み込み補充 + track store の target 書き換え)。クリップボードの
/// 旧形式 (`TracksCopy`) でも使う。`next_id` は衝突しない device id の次の値。
pub fn migrate_strips_in_track_value(track: &mut Value, next_id: &mut u64) {
    let Some(t) = track.as_object_mut() else { return };
    let strip = t.remove("strip").unwrap_or(Value::Null);
    let (comp, comp_bypassed) = take_section_on(strip.get("comp"));
    let (eq, eq_bypassed) = take_section_on(strip.get("eq"));
    let devices = t.entry("devices").or_insert_with(|| Value::Array(Vec::new()));
    let Some(devices) = devices.as_array_mut() else { return };
    let comp_id = install_builtin(devices, "Comp", comp, comp_bypassed, None, next_id);
    let eq_id = install_builtin(devices, "Eq", eq, eq_bypassed, None, next_id);
    for key in ["automation_lanes", "mod_routings"] {
        rewrite_targets(t.get_mut(key), |target| legacy_track_target(target, comp_id, eq_id));
    }
}

/// master: `master_strip.limiter` → `master_limiter`、comp / eq → `master_fx_chain` 先頭の組み込み
/// Bus Comp / Tone EQ、song store の target 書き換え。
fn migrate_master_strip(song: &mut Map<String, Value>, next: &mut u64) {
    let strip = song.remove("master_strip").unwrap_or(Value::Null);
    if let Some(limiter) = strip.get("limiter") {
        song.insert("master_limiter".to_string(), limiter.clone());
    }
    let (bus, bus_bypassed) = take_section_on(strip.get("comp"));
    let (tone, tone_bypassed) = take_section_on(strip.get("eq"));
    let chain = song.entry("master_fx_chain").or_insert_with(|| Value::Array(Vec::new()));
    let Some(chain) = chain.as_array_mut() else { return };
    let bus_id = install_builtin(chain, "BusComp", bus, bus_bypassed, Some(0), next);
    let tone_id = install_builtin(chain, "ToneEq", tone, tone_bypassed, Some(1), next);
    for key in ["song_lanes", "song_mod_routings"] {
        rewrite_targets(song.get_mut(key), |target| legacy_master_target(target, bus_id, tone_id));
    }
}

/// 旧形の印が 1 つでもあるか。
fn has_legacy_strips(song: &Value) -> bool {
    let tracks = song.get("tracks").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    if song.get("master_strip").is_some() || tracks.iter().any(|t| t.get("strip").is_some()) {
        return true;
    }
    let is_legacy = |target: &Value| {
        target.get("MasterStrip").is_some()
            || target.get("TrackBuiltin").is_some_and(|b| {
                let name = b.as_str().or_else(|| b.as_object().and_then(|o| o.keys().next().map(String::as_str)));
                matches!(name, Some("StripEqOn" | "StripCompOn" | "StripEq" | "StripComp"))
            })
    };
    let lists = tracks
        .iter()
        .flat_map(|t| [t.get("automation_lanes"), t.get("mod_routings")])
        .chain([song.get("song_lanes"), song.get("song_mod_routings")]);
    lists.flatten().filter_map(Value::as_array).flatten().filter_map(|e| e.get("target")).any(is_legacy)
}

/// 全トラックの `devices` と `master_fx_chain` にある node id (plugin / native / Parallel / chain) の最大。
fn max_node_id(song: &Map<String, Value>) -> u64 {
    fn walk(devices: &Value, max: &mut u64) {
        for d in devices.as_array().map(Vec::as_slice).unwrap_or_default() {
            let (node, parallel) = match (d.get("Parallel"), d.get("Native")) {
                (Some(p), _) => (p, true),
                (None, Some(n)) => (n, false),
                (None, None) => (d, false),
            };
            *max = (*max).max(node.get("id").and_then(Value::as_u64).unwrap_or(0));
            if !parallel {
                continue;
            }
            for chain in node.get("chains").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                *max = (*max).max(chain.get("id").and_then(Value::as_u64).unwrap_or(0));
                if let Some(inner) = chain.get("devices") {
                    walk(inner, max);
                }
            }
        }
    }
    let mut max = 0;
    for t in song.get("tracks").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        if let Some(devices) = t.get("devices") {
            walk(devices, &mut max);
        }
    }
    if let Some(chain) = song.get("master_fx_chain") {
        walk(chain, &mut max);
    }
    max
}

/// 旧セクション (comp / eq) から `on` を抜き、`(params 本体, bypassed = !on)` を返す。
/// セクションが無ければ既定値 (`{}` + bypass = 旧既定 `on: false` と同じ音)。
fn take_section_on(section: Option<&Value>) -> (Value, bool) {
    let mut body = section.and_then(Value::as_object).cloned().unwrap_or_default();
    let on = body.remove("on").and_then(|v| v.as_bool()).unwrap_or(false);
    body.remove("sc_listen");
    (Value::Object(body), !on)
}

/// 最上位の同じ種類の組み込みに値を上書き (混在形) するか、無ければ `at` (None = 末尾) に作る。
/// 戻り値 = その組み込みの device id。
fn install_builtin(devices: &mut Vec<Value>, kind: &str, params: Value, bypassed: bool, at: Option<usize>, next: &mut u64) -> u64 {
    let existing = devices.iter_mut().filter_map(|d| d.get_mut("Native")).find(|n| {
        n.get("builtin").and_then(Value::as_bool) == Some(true)
            && n.get("params").and_then(Value::as_object).is_some_and(|p| p.contains_key(kind))
    });
    if let Some(Value::Object(native)) = existing {
        let id = match native.get("id").and_then(Value::as_u64) {
            Some(id) if id != 0 => id,
            _ => alloc(next),
        };
        native.insert("id".to_string(), Value::from(id));
        native.insert("params".to_string(), json!({ kind: params }));
        native.insert("bypassed".to_string(), Value::Bool(bypassed));
        return id;
    }
    let id = alloc(next);
    let dev = json!({"Native": {"id": id, "builtin": true, "ordinal": 1, "bypassed": bypassed, "params": {kind: params}}});
    let at = at.map_or(devices.len(), |i| i.min(devices.len()));
    devices.insert(at, dev);
    id
}

fn alloc(next: &mut u64) -> u64 {
    let id = *next;
    *next = next.saturating_add(1);
    id
}

/// `list` (lane / routing の配列) の各 `target` を `f` で書き換え、`Drop` の要素を取り除く。
fn rewrite_targets(list: Option<&mut Value>, f: impl Fn(&Value) -> Rewrite) {
    let Some(items) = list.and_then(Value::as_array_mut) else { return };
    items.retain_mut(|item| {
        let Some(target) = item.get_mut("target") else { return true };
        match f(target) {
            Rewrite::Keep => true,
            Rewrite::Replace(v) => {
                *target = v;
                true
            }
            Rewrite::Drop => false,
        }
    });
}

fn native_target(device_id: u64, param: Value) -> Rewrite {
    // 住所の実在判定は新しい型の SSoT (`NativeParamId::exists`) に任せる。
    match serde_json::from_value::<NativeParamId>(param.clone()) {
        Ok(p) if p.exists() => Rewrite::Replace(json!({"NativeParam": {"device_id": device_id, "param": param}})),
        _ => Rewrite::Drop,
    }
}

/// track store の target (§6.3)。song 側の `MasterStrip` が紛れていたら削除。
fn legacy_track_target(target: &Value, comp: u64, eq: u64) -> Rewrite {
    if target.get("MasterStrip").is_some() {
        return Rewrite::Drop;
    }
    let Some(builtin) = target.get("TrackBuiltin") else { return Rewrite::Keep };
    match builtin.as_str() {
        Some("StripCompOn") => return native_target(comp, json!({"On": "Comp"})),
        Some("StripEqOn") => return native_target(eq, json!({"On": "Eq"})),
        _ => {}
    }
    if let Some(p) = builtin.get("StripComp") {
        return native_target(comp, json!({"Comp": p.get("param").cloned().unwrap_or(Value::Null)}));
    }
    if let Some(p) = builtin.get("StripEq") {
        return native_target(eq, json!({"Eq": p}));
    }
    Rewrite::Keep
}

/// song store の target (§6.3)。track 側の `Strip*` が紛れていたら削除。
fn legacy_master_target(target: &Value, bus: u64, tone: u64) -> Rewrite {
    if let Some(name) = target.get("TrackBuiltin").and_then(|b| {
        b.as_str().or_else(|| b.as_object().and_then(|o| o.keys().next().map(String::as_str)))
    }) && matches!(name, "StripEqOn" | "StripCompOn" | "StripEq" | "StripComp")
    {
        return Rewrite::Drop;
    }
    let Some(master) = target.get("MasterStrip") else { return Rewrite::Keep };
    if let Some(band) = master.get("EqGain") {
        return native_target(tone, json!({"ToneEq": band}));
    }
    let bus_param = |p: &str| native_target(bus, json!({"BusComp": p}));
    match master.as_str() {
        Some("CompOn") => native_target(bus, json!({"On": "BusComp"})),
        Some("EqOn") => native_target(tone, json!({"On": "ToneEq"})),
        Some("CompThreshold") => bus_param("Threshold"),
        Some("CompRatio") => bus_param("Ratio"),
        Some("CompAttack") => bus_param("Attack"),
        Some("CompRelease") => bus_param("Release"),
        Some("CompMakeup") => bus_param("Makeup"),
        Some("LimiterOn") => Rewrite::Replace(json!({"MasterLimiter": "On"})),
        Some("LimiterCeiling") => Rewrite::Replace(json!({"MasterLimiter": "Ceiling"})),
        _ => Rewrite::Drop,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::{
        AutomationTarget as T, BusCompAttack, BusCompParam, BusCompRatio, BusCompRelease, CompMode, CompParam,
        Device, EqBand, EqParam, MasterLimiterParam, NativeDevice, NativeKind, NativeParamId, NativeParams, Song,
    };

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v38_strips.daw")
    }

    fn fixture_song_value() -> serde_json::Value {
        let text = std::fs::read_to_string(fixture_path()).expect("read v38 fixture");
        let mut v: serde_json::Value = serde_json::from_str(&text).expect("json");
        v["song"].take()
    }

    fn natives(devices: &[Device]) -> Vec<NativeDevice> {
        let mut out = Vec::new();
        crate::model::for_each_native(devices, &mut |n| out.push(*n));
        out
    }

    fn top_ids(devices: &[Device]) -> Vec<u64> {
        devices.iter().map(Device::id).collect()
    }

    fn targets(lanes: &[crate::model::AutomationLane]) -> Vec<(u32, T)> {
        lanes.iter().map(|l| (l.id, l.target.clone())).collect()
    }

    fn np(device_id: u64, param: NativeParamId) -> T {
        T::NativeParam { device_id, param }
    }

    /// F-C10: v38 fixture を開くと、strip は実 id の組み込み native になり、target も実 id に解決する。
    #[test]
    fn v38_fixture_loads_strips_as_builtin_native_devices() {
        let loaded = crate::project::load_project(fixture_path()).expect("load v38");
        let song = loaded.song;
        // 組み込みの位置: 通常 (GWI も) は末尾に Comp → EQ、master は先頭。id は 13 から track 順 → master。
        assert_eq!(top_ids(&song.tracks[0].devices), vec![1, 2, 6, 13, 14]);
        assert_eq!(top_ids(&song.tracks[1].devices), vec![7, 8, 15, 16]);
        assert_eq!(top_ids(&song.tracks[2].devices), vec![17, 18], "devices キーが無いトラック");
        assert_eq!(top_ids(&song.tracks[3].devices), vec![19, 20]);
        assert_eq!(top_ids(&song.master_fx_chain), vec![21, 22, 9, 10]);
        for (t, want) in song.tracks.iter().zip([[false, false], [false, false], [true, true], [true, false]]) {
            let n = natives(&t.devices);
            assert!(n.iter().all(|d| d.builtin && d.ordinal == 1), "{}", t.name);
            assert_eq!([n[0].kind(), n[1].kind()], [NativeKind::Comp, NativeKind::Eq]);
            assert_eq!([n[0].bypassed, n[1].bypassed], want, "{}: bypassed = !on", t.name);
        }
        // params の一致 (Lead)。
        let lead = natives(&song.tracks[0].devices);
        let NativeParams::Comp(c) = lead[0].params else { panic!("comp") };
        assert_eq!((c.mode, c.threshold_db, c.ratio, c.attack_ms, c.release_ms, c.makeup_db, c.sc_freq_hz), (CompMode::Compressor, -18.0, 4.0, 5.0, 120.0, 3.0, 150.0));
        let NativeParams::Eq(e) = lead[1].params else { panic!("eq") };
        assert!(e.hp.on && e.hp.freq_hz == 60.0 && !e.lp.on && e.lp.freq_hz == 15_000.0 && e.hf.bell && e.hmf.gain_db == 2.5);
        // OFF の節 (Child) も値は保つ。
        let child = natives(&song.tracks[2].devices);
        let NativeParams::Comp(c) = child[0].params else { panic!("comp") };
        assert_eq!((c.mode, c.sc_freq_hz), (CompMode::Limiter, 6_000.0));
        // master。
        let master = natives(&song.master_fx_chain);
        assert_eq!([master[0].kind(), master[1].kind()], [NativeKind::BusComp, NativeKind::ToneEq]);
        assert!(!master[0].bypassed && !master[1].bypassed);
        let NativeParams::BusComp(b) = master[0].params else { panic!("bus") };
        assert_eq!((b.threshold_db, b.ratio, b.attack, b.release, b.makeup_db), (-12.0, BusCompRatio::R4, BusCompAttack::A10, BusCompRelease::Auto, 2.0));
        let NativeParams::ToneEq(tone) = master[1].params else { panic!("tone") };
        assert_eq!((tone.low_db, tone.lomid_db, tone.high_db), (1.5, -1.0, 2.0));
        assert!(song.master_limiter.on && song.master_limiter.ceiling_db == -0.5);

        // target: 実 id の NativeParam。実在しない EQ 組の lane と、その routing の深さのレーンは消える。
        assert_eq!(
            targets(&song.tracks[0].automation_lanes),
            vec![
                (1, np(14, NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain })),
                (3, T::ModRoutingDepth { routing_id: 1 }),
                (4, T::PluginParam { device_id: 4, param_id: 7, legacy_device_index: None }),
                (5, T::TrackBuiltin(crate::model::TrackBuiltinParam::ChainGain { chain_id: 5 })),
            ]
        );
        let lead_routes: Vec<(u32, T)> = song.tracks[0].mod_routings.iter().map(|r| (r.id, r.target.clone())).collect();
        assert_eq!(lead_routes, vec![(1, np(13, NativeParamId::Comp(CompParam::Ratio)))]);
        assert_eq!(
            targets(&song.tracks[1].automation_lanes),
            vec![
                (1, np(15, NativeParamId::On(NativeKind::Comp))),
                (2, np(15, NativeParamId::Comp(CompParam::Threshold))),
                (3, np(16, NativeParamId::On(NativeKind::Eq))),
            ],
            "GWI は Lead とは別の組み込みの id に解決する"
        );
        assert!(song.tracks[3].automation_lanes.is_empty(), "トラックに紛れた MasterStrip は消える");
        assert_eq!(
            targets(&song.song_lanes),
            vec![
                (1, np(21, NativeParamId::BusComp(BusCompParam::Ratio))),
                (2, T::MasterLimiter(MasterLimiterParam::On)),
                (3, T::MasterLimiter(MasterLimiterParam::Ceiling)),
                (4, T::PluginParam { device_id: 12, param_id: 3, legacy_device_index: None }),
                (6, T::ModRoutingDepth { routing_id: 3 }),
            ]
        );
        assert_eq!(song.song_mod_routings[0].target, np(21, NativeParamId::BusComp(BusCompParam::Threshold)));

        // next_device_id が遅れていても衝突しない。
        let mut all = Vec::new();
        for t in &song.tracks {
            crate::model::for_each_node_id(&t.devices, &mut |id| all.push(id));
        }
        crate::model::for_each_node_id(&song.master_fx_chain, &mut |id| all.push(id));
        let unique: std::collections::HashSet<u64> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "{all:?}");
        assert!(song.ids.next_device_id > *all.iter().max().unwrap());

        // 正規化の 2 回目で不変 / 保存 → 開き直しで等値。
        let mut again = song.clone();
        again.normalize_after_load();
        assert_eq!(again, song);
        assert!(!again.enforce_edit_invariants());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v39.daw");
        crate::project::save(&path, &song).unwrap();
        assert_eq!(crate::project::load_project(&path).unwrap().song, song);
    }

    /// F-C10: 混在形 (組み込みを既に持つ JSON に strip が載っている) は組み込みに合流し、キーが無い
    /// `master_fx_chain` / `ids` は作る。migration は冪等。
    #[test]
    fn migration_merges_mixed_form_and_creates_missing_keys() {
        let mut once = fixture_song_value();
        super::migrate_strips_to_native(&mut once);
        let snapshot = once.clone();
        super::migrate_strips_to_native(&mut once);
        assert_eq!(once, snapshot, "新形は触らない");

        let mut mixed = snapshot.clone();
        mixed["tracks"][0]["strip"] = serde_json::json!({"comp": {"on": false, "threshold_db": -30.0}, "eq": {"on": true}});
        mixed["master_strip"] = serde_json::json!({"comp": {"on": false, "threshold_db": -6.0}});
        super::migrate_strips_to_native(&mut mixed);
        let song: Song = serde_json::from_value(mixed).expect("deserialize");
        let lead = natives(&song.tracks[0].devices);
        assert_eq!(lead.iter().map(|n| n.id).collect::<Vec<_>>(), vec![13, 14], "同じ組み込みに合流 (増えない)");
        assert!(lead[0].bypassed && !lead[1].bypassed);
        assert!(matches!(lead[0].params, NativeParams::Comp(c) if c.threshold_db == -30.0));
        let master = natives(&song.master_fx_chain);
        assert_eq!(master.iter().map(|n| n.id).collect::<Vec<_>>(), vec![21, 22]);
        assert!(matches!(master[0].params, NativeParams::BusComp(b) if b.threshold_db == -6.0) && master[0].bypassed);

        let mut bare = fixture_song_value();
        let obj = bare.as_object_mut().unwrap();
        obj.remove("master_fx_chain");
        obj.remove("ids");
        super::migrate_strips_to_native(&mut bare);
        let chain = bare["master_fx_chain"].as_array().expect("master_fx_chain を作る");
        assert_eq!(chain.len(), 2);
        assert_eq!(bare["ids"]["next_device_id"], 19, "master chain を消すと最大 node id は 8 → 9 から 10 個");
    }

    /// F-C11: script と同じ経路 (`migrate_legacy_song` + `from_value` + `ensure_ids`) でも同じ解決になる。
    #[test]
    fn script_load_path_resolves_the_same_as_file_load() {
        let file = crate::project::load_project(fixture_path()).unwrap().song;
        let mut value = fixture_song_value();
        crate::project::migrate_legacy_song(&mut value);
        crate::project::tag_clip_contents_in_song(&mut value);
        let mut script: Song = serde_json::from_value(value).unwrap();
        script.ensure_ids();
        for (a, b) in script.tracks.iter().zip(&file.tracks) {
            assert_eq!(a.devices, b.devices, "{}", a.name);
            assert_eq!(a.mod_routings, b.mod_routings, "{}", a.name);
        }
        assert_eq!(script.master_fx_chain, file.master_fx_chain);
        assert_eq!(script.master_limiter, file.master_limiter);
        // 深さのレーンの連鎖掃除は SongDoc の口 (`enforce_edit_invariants`) が担う。
        script.enforce_edit_invariants();
        for (a, b) in script.tracks.iter().zip(&file.tracks) {
            assert_eq!(targets(&a.automation_lanes), targets(&b.automation_lanes), "{}", a.name);
        }
        assert_eq!(targets(&script.song_lanes), targets(&file.song_lanes));
    }
}
