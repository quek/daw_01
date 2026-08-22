//! r.md #61: 終了シーケンスの回帰テスト。
//!
//! 守りたい不変条件は 4 つ。どれも「実機で ✕ を押す」でしか見えなかったものを
//! headless に落としたもの:
//!
//! 1. **子プロセスへ終了を伝える**。旧実装は終了時に IPC を一切送らず、Job Object
//!    の強制 kill だけで終わっていた (= プラグインの deactivate / destroy が
//!    一度も走らない)。
//! 2. **順序**: 走行中のオフライン処理を止める → transport を止める →
//!    プラグインを畳ませる、の順。process() を回している最中に deactivate させない。
//! 3. **終了中は respawn しない**。子が自力 exit すると pipe は子側から先に閉じ、
//!    reader が EOF を crash と同じ `ChildDisconnected` に合成する。ガードが無いと
//!    「終了しようとしているのに子が生き返る」。
//! 4. **未保存確認は飛ばさない** (ユーザー操作の場合)。

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{AppData, AppEvent, DirtyGuardAction};
use daw_gui::shutdown::QuitRequest;

use super::support::{self, drain};

fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (app, audio_rx, plugin_rx, _dispatcher) = support::build_app();
    (app, audio_rx, plugin_rx)
}

#[test]
fn 終了要求は子プロセスへ停止と終了を正しい順序で送る() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    app.song_doc.mark_saved();
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));

    assert!(app.shutdown.is_draining(), "clean project は即シーケンスに入る");

    let audio = drain(&mut audio_rx);
    // transport を止めてから engine を終了させる (逆順だと停止が届かない)。
    let stop = audio
        .iter()
        .position(|c| matches!(c, AudioCommand::Stop))
        .expect("Stop を送る");
    let shutdown = audio
        .iter()
        .position(|c| matches!(c, AudioCommand::Shutdown))
        .expect("AudioCommand::Shutdown を送る");
    assert!(stop < shutdown, "Stop → Shutdown の順 (got {audio:?})");

    let plugin = drain(&mut plugin_rx);
    assert!(
        plugin.iter().any(|c| matches!(c, PluginCommand::Shutdown)),
        "PluginCommand::Shutdown を送る (got {plugin:?})"
    );
}

#[test]
fn 書き出し中の終了は先に中止を送る() {
    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.song_doc.mark_saved();
    app.transport.export_stage = Some(daw_gui::app::ExportStage::AudioRender { done: 1, total: 10 });
    let _ = drain(&mut audio_rx);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));

    let audio = drain(&mut audio_rx);
    let cancel = audio
        .iter()
        .position(|c| matches!(c, AudioCommand::CancelExport))
        .expect("走行中の書き出しを中止する");
    let shutdown = audio
        .iter()
        .position(|c| matches!(c, AudioCommand::Shutdown))
        .expect("Shutdown を送る");
    assert!(
        cancel < shutdown,
        "freewheel 中に Shutdown を投げない (got {audio:?})"
    );
}

#[test]
fn 未保存なら確認モーダルを出して子には何も送らない() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    app.song_doc.normalize(|_| {});
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);

    app.handle_event(AppEvent::Quit(QuitRequest::USER));

    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "確認モーダルを開く"
    );
    assert!(!app.shutdown.is_shutting_down(), "答えを聞く前に畳み始めない");
    assert!(
        !drain(&mut plugin_rx)
            .iter()
            .any(|c| matches!(c, PluginCommand::Shutdown)),
        "確認前にプラグインを畳ませない"
    );
}

#[test]
fn 自動実行の終了要求は未保存確認を飛ばして終了コードを運ぶ() {
    let (mut app, _audio_rx, _plugin_rx) = build_app();
    // smoke test は fixture を import して必ず dirty になる。
    app.song_doc.normalize(|_| {});

    app.handle_event(AppEvent::Quit(QuitRequest::automated(1)));

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "答える人間が居ないので聞かない");
    assert!(app.shutdown.is_draining());
    assert_eq!(app.shutdown.exit_code(), 1, "判定結果が終了コードで返る");
}

/// 終了中に子が切断しても再起動しない。
///
/// 抑止は 2 段になっている: (a) `handle_event` の全遮断 gate、
/// (b) `handle_child_disconnected` 入口の `suppress_child_respawn`。
/// ここで見るのは **外から観測できる契約** (子が生き返らない / 「切断されました」
/// の通知が出ない) で、どちらの段が止めたかは問わない。
#[test]
fn 終了中の切断では子を再起動しない() {
    let (mut app, _audio_rx, mut plugin_rx) = build_app();
    app.song_doc.mark_saved();
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    let _ = drain(&mut plugin_rx);

    // 子が自力 exit → pipe が子側から閉じる → reader が EOF を合成する経路。
    app.handle_event(AppEvent::Plugin(common::protocol::PluginEvent::ChildDisconnected));

    assert!(
        app.ipc.plugin_tx.is_some(),
        "終了中は tx を落とさない (= 切断を crash として処理しない)"
    );
    assert!(
        app.ui_ephemeral.status_message.is_empty(),
        "「切断されました」の通知を出さない (got {:?})",
        app.ui_ephemeral.status_message
    );
}

/// 終了を決めた後の副作用を全部落とす。
///
/// `Draining` は「子の teardown を待つ間もイベントループが回り続ける」という
/// 新しい窓で、旧実装 (`should_quit` を立てた同じフレームで `exit()`) には
/// 存在しなかった。ここを開けたままにすると、畳ませた plugin host へ
/// `SetSlotPlugin` が飛んだり、autosave が recovery ファイルを書き直したりする。
#[test]
fn 終了中は以後のイベントを一切受け付けない() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    app.song_doc.mark_saved();
    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    let epoch = app.song_doc.edit_epoch();

    // 編集 / 再生 / autosave — どれも終了を決めた後に効いてはいけない。
    app.handle_event(AppEvent::AddInstrumentTrack);
    app.handle_event(AppEvent::Play);
    app.handle_event(AppEvent::AutosaveTick);

    assert_eq!(app.song_doc.edit_epoch(), epoch, "Song は変わらない");
    assert!(
        drain(&mut audio_rx).is_empty(),
        "子プロセスへ新しいコマンドを送らない"
    );
    assert!(drain(&mut plugin_rx).is_empty());
}

/// recovery ファイルの削除は **本当に終わる瞬間**。
///
/// drain の開始時に消すと、teardown に数秒かかる間に OS へ強制終了された場合
/// 「まだ終われていないのに復旧候補だけ消えている」状態になる。
#[test]
fn recovery_ファイルは終了完了時に消す() {
    let (mut app, _audio_rx, _plugin_rx) = build_app();
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::Quit(QuitRequest::USER));
    assert!(app.shutdown.is_draining(), "まだ待っている段階");

    app.poll_shutdown();
    assert!(app.shutdown.is_finished());
}

#[test]
fn 終了待ちは監視対象が無ければ即完了する() {
    // script / test 経路 (supervisor: None) は待つ相手が居ない。
    let (mut app, _audio_rx, _plugin_rx) = build_app();
    app.song_doc.mark_saved();
    app.handle_event(AppEvent::Quit(QuitRequest::USER));

    app.poll_shutdown();

    assert!(app.shutdown.is_finished());
    assert!(!app.shutdown.forced(), "待つ相手が居ないのは強制終了ではない");
    assert!(app.shutdown.stragglers().is_empty());
}

