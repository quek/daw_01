//! r.md #78: 変調ルート指定を「◉ (arm) のワンショット」1 本に統一した挙動の回帰。
//!
//! ここで固定するのは、 候補 dropdown を撤去した結果として **唯一の指定経路**に
//! なった 3 つの契約:
//!
//! 1. 待受中に **プラグイン自身の窓の中**でツマミを触ると routing ができ、 待受が
//!    自動解除される (`PluginParamTouched` → `connect_armed_mod_source_to`)。
//!    daw_gui はプラグインの窓に overlay を描けないので、 touch 通知が
//!    「窓の中のツマミ」への唯一の到達手段。
//! 2. 待受していなければ触っても何も起きない (音作りでツマミをいじっただけで
//!    繋がる事故を起こさない)。
//! 3. ソース所有トラック **以外** のツマミに繋いだ routing も、 ソース側の
//!    ラック行に必ず並ぶ (`mod_source_routings`)。 旧実装ではカーソルトラックの
//!    routing しか描かず、 この組み合わせは表示も削除もできない孤児だった。
//!
//! 併せて r.md #72 (「Dry/Wet」がどのプラグインのものか分からない) の SSoT —
//! `automation_target_label` がデバイス名を前置きすること — も固定する。

use common::model::{
    AudioTap, AutomationTarget, FollowerConfig, ModSource, ModSourceKind, PluginInstance, Track,
};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::{PluginEvent, PluginParamInfo};

use daw_gui::app::{AppData, AppEvent};

use super::support::build_app;

const TRACK_A: u32 = 100;
const TRACK_B: u32 = 200;
const DEVICE_A: u64 = 11;
const DEVICE_B: u64 = 22;
const PARAM_ID: u32 = 7;

fn fx_track(id: u32, name: &str, device_id: u64, plugin_id: &str) -> Track {
    let mut track = Track { id, name: name.into(), ..Track::default() };
    track.devices.push(PluginInstance {
        id: device_id,
        ..PluginInstance::with_ports(
            plugin_id.to_string(),
            PluginFormat::Clap,
            PortConfig { has_audio_input: true, has_audio_output: true, ..Default::default() },
        )
    });
    track
}

/// 2 トラック (どちらも FX device 1 個) と、 TRACK_A 所有の LFO ソースを 1 個持つ
/// app を作る。 host からの `PluginParamList` も両 device 分 fake dispatch して、
/// param 名が解決できる状態にする (= 実機で activate 直後に届くのと同じ)。
fn build_app_with_two_fx_tracks() -> (AppData, u32) {
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(fx_track(TRACK_A, "Lead", DEVICE_A, "test.delay"));
        song.tracks.push(fx_track(TRACK_B, "Drums", DEVICE_B, "test.bitcrush"));
        let id = song.alloc_mod_source_id();
        song.mod_sources.push(ModSource {
            id,
            owner_track_id: TRACK_A,
            color: [0.3, 0.7, 1.0],
            kind: ModSourceKind::EnvelopeFollower {
                tap: AudioTap::post_fader(TRACK_A),
                follower: FollowerConfig::default(),
            },
        });
    });
    for device_id in [DEVICE_A, DEVICE_B] {
        app.handle_event(AppEvent::Plugin(PluginEvent::PluginParamList {
            device_id,
            params: vec![PluginParamInfo {
                id: PARAM_ID,
                name: "Dry/Wet".into(),
                module: String::new(),
                min_value: 0.0,
                max_value: 1.0,
                default_value: 0.5,
                flags: 0,
            }],
            has_embedded_gui: true,
        }));
    }
    let source_id = app.song_doc.song().mod_sources[0].id;
    assert_ne!(source_id, 0, "mod_source に安定 id が採番されている");
    (app, source_id)
}

fn touch_plugin_knob(app: &mut AppData, device_id: u64) {
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginParamTouched {
        device_id,
        param_id: PARAM_ID,
        // host が送るのは placeholder。 daw_gui 側が実名で上書きする。
        display_name: format!("Param {PARAM_ID}"),
    }));
}

fn routings_of(app: &AppData, track_id: u32) -> Vec<(u32, AutomationTarget)> {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .map(|t| {
            t.mod_routings
                .iter()
                .map(|r| (r.source_id, r.target.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// 待受中にプラグイン窓のツマミを触ると routing ができ、 待受は自動解除される。
#[test]
fn arm_then_plugin_knob_touch_creates_routing_and_disarms() {
    let (mut app, source_id) = build_app_with_two_fx_tracks();
    app.handle_event(AppEvent::SetArmedModSource(Some(source_id)));

    touch_plugin_knob(&mut app, DEVICE_A);

    let routings = routings_of(&app, TRACK_A);
    assert_eq!(routings.len(), 1, "触った param に routing が 1 本できる: {routings:?}");
    assert_eq!(routings[0].0, source_id);
    assert_eq!(
        routings[0].1,
        AutomationTarget::PluginParam {
            device_id: DEVICE_A,
            param_id: PARAM_ID,
            legacy_device_index: None,
        }
    );
    assert_eq!(
        app.ui_ephemeral.armed_mod_source, None,
        "1 本繋いだら待受は自動解除される (待受けたまま忘れて誤爆しない)"
    );
}

/// 待受していなければ、 同じ touch は routing を作らない。
#[test]
fn plugin_knob_touch_without_arm_creates_nothing() {
    let (mut app, _source_id) = build_app_with_two_fx_tracks();

    touch_plugin_knob(&mut app, DEVICE_A);

    assert!(
        routings_of(&app, TRACK_A).is_empty(),
        "待受していないときの touch は音作りの操作であって割り当てではない"
    );
}

/// 解除後にもう一度触っても繋がらない (= 自動解除が本当に効いている)。
#[test]
fn second_touch_after_auto_disarm_does_not_add_another_routing() {
    let (mut app, source_id) = build_app_with_two_fx_tracks();
    app.handle_event(AppEvent::SetArmedModSource(Some(source_id)));
    touch_plugin_knob(&mut app, DEVICE_A);

    // 別 device のツマミを触る = 続けて割り当てられてしまわないこと。
    touch_plugin_knob(&mut app, DEVICE_B);

    assert!(
        routings_of(&app, TRACK_B).is_empty(),
        "自動解除後の touch は繋がらない"
    );
}

/// ソース所有トラック以外の param に繋いだ routing も、 ソース側の行に並ぶ。
/// 旧実装ではこの組み合わせがどちらのインスペクタにも出ず削除できなかった。
#[test]
fn cross_track_routing_is_listed_under_its_source() {
    let (mut app, source_id) = build_app_with_two_fx_tracks();
    app.handle_event(AppEvent::SetArmedModSource(Some(source_id)));

    // TRACK_A 所有のソースを待受にしたまま、 TRACK_B の device のツマミを触る。
    touch_plugin_knob(&mut app, DEVICE_B);

    assert!(
        routings_of(&app, TRACK_A).is_empty(),
        "routing は対象を持つトラック側に載る (engine がそのトラックの routings を読むため)"
    );
    assert_eq!(routings_of(&app, TRACK_B).len(), 1);

    let rows = app.mod_source_routings(source_id);
    assert_eq!(rows.len(), 1, "ソース側から見て 1 本: {rows:?}");
    assert_eq!(rows[0].track_id, TRACK_B, "削除・depth 編集の宛先は対象トラック");
    assert!(
        rows[0].label.starts_with("Drums \u{25b8} "),
        "他トラック宛はトラック名を前置きする: {}",
        rows[0].label
    );
}

/// r.md #72: 同名 param をデバイス名で区別できる。 ラベルの SSoT は
/// `automation_target_label` 1 本なので、 ラックの接続行も arrangement の
/// オートメーションレーン名も同時にこの形になる。
#[test]
fn plugin_param_label_is_device_qualified() {
    let (app, _source_id) = build_app_with_two_fx_tracks();
    let label_a = app.automation_target_label(&AutomationTarget::PluginParam {
        device_id: DEVICE_A,
        param_id: PARAM_ID,
        legacy_device_index: None,
    });
    let label_b = app.automation_target_label(&AutomationTarget::PluginParam {
        device_id: DEVICE_B,
        param_id: PARAM_ID,
        legacy_device_index: None,
    });
    assert_eq!(label_a, "Test Delay: Dry/Wet");
    assert_eq!(label_b, "Test Bitcrush: Dry/Wet");
    assert_ne!(
        label_a, label_b,
        "同名 param がデバイス違いで区別できる (これが r.md #72 の要求)"
    );
}

/// ソースを所有するトラックを消したら、 ソース本体とその接続も道連れになる。
/// ソースはラックで所有トラックの下にしか出ないので、 残すと**どこからも削除
/// できないまま**生き残ったトラックを変調し続ける。
#[test]
fn deleting_owner_track_removes_its_mod_source_and_routings() {
    // プラグインを持たない 2 トラックで組む。 device があると `delete_tracks` が
    // plugin state の round-trip 待ちで deferred になり、 同期テストで観測できない。
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    let source_id = app
        .edit_song(|song| {
            song.tracks.clear();
            song.tracks
                .push(Track { id: TRACK_A, name: "Lead".into(), ..Track::default() });
            song.tracks
                .push(Track { id: TRACK_B, name: "Drums".into(), ..Track::default() });
            let id = song.alloc_mod_source_id();
            song.mod_sources.push(ModSource {
                id,
                owner_track_id: TRACK_A,
                color: [0.3, 0.7, 1.0],
                kind: ModSourceKind::default(),
            });
            id
        })
        .expect("edit_song");
    // TRACK_A 所有のソースを TRACK_B の音量へ繋ぐ (= 生き残る側に接続が残る形)。
    app.handle_event(AppEvent::AddModRouting {
        track_id: TRACK_B,
        target: AutomationTarget::TrackBuiltin(common::model::TrackBuiltinParam::Volume),
        source_id,
    });
    assert_eq!(routings_of(&app, TRACK_B).len(), 1);

    app.handle_event(AppEvent::DeleteTracks(vec![TRACK_A]));

    assert!(
        app.song_doc.song().mod_sources.is_empty(),
        "所有トラックと一緒にソースも消える"
    );
    assert!(
        routings_of(&app, TRACK_B).is_empty(),
        "生き残ったトラックに残った接続も掃除される (幽霊変調にしない)"
    );
}

/// ソースを削除したら待受も解除される (削除済み id を掴んだままだと、 次に触った
/// ツマミが幽霊 routing になる)。
#[test]
fn removing_armed_source_disarms() {
    let (mut app, source_id) = build_app_with_two_fx_tracks();
    app.handle_event(AppEvent::SetArmedModSource(Some(source_id)));

    app.handle_event(AppEvent::RemoveModSource { id: source_id });

    assert_eq!(app.ui_ephemeral.armed_mod_source, None);
    touch_plugin_knob(&mut app, DEVICE_A);
    assert!(routings_of(&app, TRACK_A).is_empty());
}
