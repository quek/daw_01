//! r.md #88: エンベロープフォロワーの `gain` / `mode` / `rectify` / `band_filter` に
//! 編集経路を足した件の回帰。
//!
//! ここで固定するのは、 値そのものではなく **値からは読めない 2 つの契約**:
//!
//! 1. **帯域は逆転しない** — `hp` を `lp` より上へ動かすと `lp` が付いてくる。
//!    一次フィルタ 2 段が互いを打ち消した帯域は「無音を検出し続ける」 形で壊れ、
//!    しかもその状態は表示された数値からは読めない。
//! 2. **「触った parameter」は動いた側だけ** — `hp` を触ったら `hp`、 `lp` を触ったら
//!    `lp`。 帯域の on/off は値の編集ではないので記録しない。 これを間違えると
//!    `A` キーが常に片方のレーンを作る / どのツマミでもレーンを作れない、 になる。
//!    どちらもコンパイルは通り、 実機で触るまで気付けない。

use common::model::{
    AudioTap, AutomationTarget, BandFilter, FollowerConfig, FollowerMode, ModParam, ModSource,
    ModSourceKind, Track,
};

use daw_gui::app::{AppData, AppEvent};

use super::support::build_app;

const TRACK_A: u32 = 100;

fn build_app_with_follower() -> (AppData, u32) {
    let (mut app, _audio_rx, _plugin_rx, _disp) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(Track { id: TRACK_A, name: "Lead".into(), ..Track::default() });
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
    let source_id = app.song_doc.song().mod_sources[0].id;
    (app, source_id)
}

fn follower(app: &AppData, source_id: u32) -> FollowerConfig {
    app.song_doc
        .song()
        .mod_sources
        .iter()
        .find(|m| m.id == source_id)
        .and_then(|m| m.follower().map(|(_, f)| *f))
        .expect("follower")
}

fn touched(app: &AppData) -> Option<AutomationTarget> {
    app.ui_ephemeral.last_touched_param.as_ref().map(|t| t.target.clone())
}

/// 新しい 4 つの口が実際にモデルへ効き、 帯域が逆転しない。
#[test]
fn follower_knobs_edit_the_model_and_band_never_inverts() {
    let (mut app, sid) = build_app_with_follower();

    app.handle_event(AppEvent::SetModSourceGain { id: sid, gain: 2.5 });
    app.handle_event(AppEvent::SetModSourceMode { id: sid, mode: FollowerMode::Rms });
    app.handle_event(AppEvent::SetModSourceRectify { id: sid, rectify: false });
    let f = follower(&app, sid);
    assert!((f.gain - 2.5).abs() < 1e-6, "gain が入る (got {})", f.gain);
    assert_eq!(f.mode, FollowerMode::Rms);
    assert!(!f.rectify);

    // 範囲外の gain は端で止まる (ツマミの端 == 変調の端)。
    app.handle_event(AppEvent::SetModSourceGain { id: sid, gain: 1_000.0 });
    let hi = follower(&app, sid).gain;
    assert!((hi - common::model::MOD_FOLLOWER_GAIN_MAX).abs() < 1e-6, "上端で止まる (got {hi})");

    // 帯域を入れてから hp を lp より上へ動かすと、 lp が付いてくる (逆転しない)。
    app.handle_event(AppEvent::SetModSourceBand {
        id: sid,
        band: Some(BandFilter { hp_hz: 30.0, lp_hz: 200.0 }),
    });
    app.handle_event(AppEvent::SetModSourceBand {
        id: sid,
        band: Some(BandFilter { hp_hz: 5_000.0, lp_hz: 200.0 }),
    });
    let b = follower(&app, sid).band_filter.expect("band");
    assert!(b.lp_hz >= b.hp_hz, "hp {} <= lp {} を保つ", b.hp_hz, b.lp_hz);

    // 帯域を外すと全帯域に戻る。
    app.handle_event(AppEvent::SetModSourceBand { id: sid, band: None });
    assert!(follower(&app, sid).band_filter.is_none());
}

/// 「触った parameter」は **動いた側だけ**。 on/off は値ではないので記録しない。
#[test]
fn band_records_only_the_cutoff_that_moved() {
    let (mut app, sid) = build_app_with_follower();

    // (1) 帯域を ON にしただけでは記録しない (値の編集ではない)。
    app.handle_event(AppEvent::SetModSourceBand {
        id: sid,
        band: Some(BandFilter { hp_hz: 30.0, lp_hz: 200.0 }),
    });
    assert_eq!(touched(&app), None, "on/off は「触った parameter」ではない");

    // (2) hp だけ動かす → hp が記録される。
    app.handle_event(AppEvent::SetModSourceBand {
        id: sid,
        band: Some(BandFilter { hp_hz: 60.0, lp_hz: 200.0 }),
    });
    assert_eq!(
        touched(&app),
        Some(AutomationTarget::ModSourceParam { source_id: sid, param: ModParam::FollowerHpHz }),
    );

    // (3) lp だけ動かす → lp が記録される (hp のまま据え置かない)。
    app.handle_event(AppEvent::SetModSourceBand {
        id: sid,
        band: Some(BandFilter { hp_hz: 60.0, lp_hz: 800.0 }),
    });
    assert_eq!(
        touched(&app),
        Some(AutomationTarget::ModSourceParam { source_id: sid, param: ModParam::FollowerLpHz }),
    );

    // (4) gain / attack / release も同じ口を通る。
    app.handle_event(AppEvent::SetModSourceGain { id: sid, gain: 2.0 });
    assert_eq!(
        touched(&app),
        Some(AutomationTarget::ModSourceParam { source_id: sid, param: ModParam::FollowerGain }),
    );
}
