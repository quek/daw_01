//! マスターストリップ (docs/plan_master_strip.md) の編集経路の headless 回帰。
//!
//! 1. 段階式パラメータが段へ丸められて `Song` に載り、audio へ `SetMasterStrip` が飛ぶ
//! 2. 中身を触ったブロックは自動で ON になる (バイパスそのものの操作は除く)
//! 3. リミッターのシーリングが可動範囲へ丸められる

use std::sync::Arc;

use common::model::{MasterRatio, MasterRelease, MasterStrip, MasterStripParam};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

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

fn strip(app: &AppData) -> MasterStrip {
    app.song_doc.song().master_strip
}

fn drain(rx: &mut UnboundedReceiver<AudioCommand>) -> Vec<MasterStrip> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let AudioCommand::SetMasterStrip { strip } = cmd {
            out.push(strip);
        }
    }
    out
}

#[test]
fn 段階式パラメータは段へ丸められて_audio_まで届く() {
    let (mut app, mut audio_rx, _p) = build_app();

    // Ratio は index ドメイン (0=2:1 / 1=4:1 / 2=10:1)。1.4 は 4:1 へ丸まる。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::CompRatio,
        value: 1.4,
    });
    assert_eq!(strip(&app).comp.ratio, MasterRatio::R4);

    // Release の最上段は Auto。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::CompRelease,
        value: 4.0,
    });
    assert_eq!(strip(&app).comp.release, MasterRelease::Auto);

    let sent = drain(&mut audio_rx);
    assert_eq!(sent.len(), 2, "編集ごとに 1 通ずつ飛ぶ");
    assert_eq!(sent[1].comp.release, MasterRelease::Auto);
}

#[test]
fn 中身を触ったブロックは自動で_on_になる() {
    let (mut app, _a, _p) = build_app();
    assert!(strip(&app).is_bypassed(), "既定は全バイパス");

    // コンプのノブ → コンプだけ ON。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::CompThreshold,
        value: -12.0,
    });
    assert!(strip(&app).comp.on);
    assert!(!strip(&app).eq.on);
    assert!(!strip(&app).limiter.on);

    // EQ のノブ → EQ が ON。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::EqGain(common::model::MasterEqBand::Low),
        value: 3.0,
    });
    assert!(strip(&app).eq.on);

    // シーリング → リミッターが ON。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::LimiterCeiling,
        value: -0.5,
    });
    assert!(strip(&app).limiter.on);

    // バイパスそのものの操作 (= Q キー) は自動 ON に巻き戻されない。
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::CompOn,
        value: 0.0,
    });
    assert!(!strip(&app).comp.on, "OFF にしたのに戻っている");
}

#[test]
fn シーリングは可動範囲へ丸められる() {
    let (mut app, _a, _p) = build_app();
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::LimiterCeiling,
        value: 5.0,
    });
    assert!((strip(&app).limiter.ceiling_db - 0.0).abs() < 1e-6);
    app.handle_event(AppEvent::MasterStripEdit {
        param: MasterStripParam::LimiterCeiling,
        value: -99.0,
    });
    assert!((strip(&app).limiter.ceiling_db - -6.0).abs() < 1e-6);
}
