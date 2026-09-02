//! 内蔵チャンネルストリップ (docs/plan_channel_strip.md) の編集経路の headless 回帰。
//!
//! 見ているのは「ノブを回すと何が起きるか」の 3 点:
//! 1. 値が可動範囲へ丸められて `Song` に載り、audio へ `SetTrackStrip` が飛ぶ
//! 2. `SC Listen` は **同時に 1 トラックだけ** (solo と同じ排他)
//! 3. セクションの開閉は `UiPrefs` だけを動かし、曲を dirty にしない

use std::sync::Arc;

use common::model::{ChannelStrip, CompParam, EqBand, EqParam, TrackBuiltinParam};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event::{StripEdit, StripSection, StripSwitch};

fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    (app, audio_rx, plugin_rx)
}

fn strip(app: &AppData, track_id: u32) -> ChannelStrip {
    app.song_doc.song().track_by_id(track_id).expect("track exists").strip
}

/// audio へ飛んだ `SetTrackStrip` を全部拾う。
fn drain_strip_cmds(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<(u32, ChannelStrip)> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let AudioCommand::SetTrackStrip { track, strip } = cmd {
            out.push((track, strip));
        }
    }
    out
}

#[test]
fn ノブの値は可動範囲へ丸められて_audio_まで届く() {
    let (mut app, mut audio_rx, _p) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;

    // HMF の Freq は 400Hz〜8kHz。範囲外を投げても端で止まる。
    app.handle_event(AppEvent::StripEdit {
        track: track_id,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripEq { band: EqBand::Hmf, param: EqParam::Freq },
            value: 50_000.0,
        },
    });
    let s = strip(&app, track_id);
    assert!(
        (s.eq.param(EqBand::Hmf, EqParam::Freq) - 8_000.0).abs() < 1e-3,
        "freq={}",
        s.eq.param(EqBand::Hmf, EqParam::Freq)
    );

    // 検出フィルタは 20Hz 未満を OFF (0.0) に落とす。
    app.handle_event(AppEvent::StripEdit {
        track: track_id,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripComp { param: CompParam::ScFreq },
            value: 3.0,
        },
    });
    assert_eq!(strip(&app, track_id).comp.sc_freq_hz, 0.0);

    let sent = drain_strip_cmds(&mut audio_rx);
    assert_eq!(sent.len(), 2, "編集ごとに 1 通ずつ飛ぶ");
    assert_eq!(sent[1].0, track_id);
    assert_eq!(sent[1].1.comp.sc_freq_hz, 0.0);
}

#[test]
fn 中身を触ったセクションは自動で_on_になる() {
    let (mut app, _a, _p) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    assert!(!strip(&app, track_id).eq.on, "既定はバイパス");
    assert!(!strip(&app, track_id).comp.on);

    // EQ のノブ → EQ だけが ON になる (コンプは巻き込まない)。
    app.handle_event(AppEvent::StripEdit {
        track: track_id,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripEq { band: EqBand::Hf, param: EqParam::Gain },
            value: 3.0,
        },
    });
    assert!(strip(&app, track_id).eq.on, "EQ を触ったのに ON にならない");
    assert!(!strip(&app, track_id).comp.on, "コンプまで ON になっている");

    // コンプのノブ → コンプが ON。
    app.handle_event(AppEvent::StripEdit {
        track: track_id,
        edit: StripEdit::Param {
            param: TrackBuiltinParam::StripComp { param: CompParam::Threshold },
            value: -12.0,
        },
    });
    assert!(strip(&app, track_id).comp.on);

    // バイパスバッジ自体の操作は「明示指定」なので自動 ON に巻き戻されない。
    app.handle_event(AppEvent::StripEdit {
        track: track_id,
        edit: StripEdit::Param { param: TrackBuiltinParam::StripEqOn, value: 0.0 },
    });
    assert!(!strip(&app, track_id).eq.on, "OFF にしたのに戻っている");
}

#[test]
fn sc_listen_は同時に一トラックだけ() {
    let (mut app, mut audio_rx, _p) = build_app();
    app.handle_event(AppEvent::AddInstrumentTrack);
    let ids: Vec<u32> = app.song_doc.song().tracks.iter().map(|t| t.id).collect();
    assert!(ids.len() >= 2, "2 トラック以上で試す");
    let (a, b) = (ids[0], ids[1]);

    app.handle_event(AppEvent::StripEdit {
        track: a,
        edit: StripEdit::Switch { switch: StripSwitch::ScListen, on: true },
    });
    assert!(strip(&app, a).comp.sc_listen);

    let _ = drain_strip_cmds(&mut audio_rx);
    app.handle_event(AppEvent::StripEdit {
        track: b,
        edit: StripEdit::Switch { switch: StripSwitch::ScListen, on: true },
    });
    assert!(strip(&app, b).comp.sc_listen);
    assert!(!strip(&app, a).comp.sc_listen, "前のトラックの試聴が消えていない");

    // 消えた側も audio へ届いていないと、engine 側では 2 本鳴ったままになる。
    let sent = drain_strip_cmds(&mut audio_rx);
    let for_a = sent.iter().find(|(t, _)| *t == a).expect("解除も送られる");
    assert!(!for_a.1.comp.sc_listen);
}

#[test]
fn セクションの開閉は曲を汚さない() {
    let (mut app, _a, _p) = build_app();
    assert!(!app.song_doc.is_dirty(), "初期状態は clean");
    assert!(!app.ui_prefs.strip_eq_open);

    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    assert!(app.ui_prefs.strip_eq_open);
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    assert!(app.ui_prefs.strip_comp_open);
    assert!(!app.song_doc.is_dirty(), "見方の都合で `*` が立ってはいけない");

    app.handle_event(AppEvent::ToggleStripSection(StripSection::Eq));
    assert!(!app.ui_prefs.strip_eq_open);
}
