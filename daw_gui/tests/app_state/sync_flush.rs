// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! S3b-i (docs/plan_arch_refactor.md §7.5「sync 一本化」): 子プロセス sync を
//! epoch ベースの pull 型に一本化した挙動を検証する。 実機では runner が frame 末に
//! `flush_song_sync` を 1 回呼ぶ; headless test はその frame 境界を明示的に模す。
//!
//! flush の 1 フレーム遅延で壊れうる順序依存を 2 面で押さえる:
//! (a) 1 frame に複数編集 → flush は LoadSong を 1 回だけ送る (coalesce)。
//! (b) 編集直後の Play が最新 song を先に送る (ensure-synced)。

use common::protocol::AudioCommand;

use daw_gui::app::AppEvent;

use super::support::{build_app, drain};

fn count_loadsong(msgs: &[AudioCommand]) -> usize {
    msgs.iter().filter(|m| matches!(m, AudioCommand::LoadSong(_))).count()
}

/// (a) 1 frame に複数編集を積んでも、 flush は LoadSong を **1 回だけ** 送る
/// (epoch 差分での coalesce)。 編集の無い再 flush は epoch 一致で no-op。
#[test]
fn multiple_edits_in_one_frame_coalesce_to_single_loadsong() {
    let (mut app, mut audio_rx, _plugin_rx, _proxy) = build_app();
    // baseline: 初回 flush で初期 song を送り切り、 以後の LoadSong だけを観測する。
    app.flush_song_sync();
    let _ = drain(&mut audio_rx);

    // 1 frame 内で複数編集 (それぞれ edit_epoch を bump)。 pull 型なので、 編集自体は
    // LoadSong を送らない (旧: 各編集が即 sync_song_to_plugin_host で LoadSong flood)。
    let _ = app.edit_song(|s| s.bpm = 130.0);
    let _ = app.edit_song(|s| s.bpm = 140.0);
    let _ = app.edit_song(|s| s.bpm = 150.0);
    let before_flush = drain(&mut audio_rx);
    assert_eq!(
        count_loadsong(&before_flush),
        0,
        "編集だけでは LoadSong は送られない (pull 型): {before_flush:?}"
    );

    // frame flush 1 回 → 3 編集が 1 回の LoadSong に coalesce される。
    app.flush_song_sync();
    let after_flush = drain(&mut audio_rx);
    assert_eq!(
        count_loadsong(&after_flush),
        1,
        "1 frame の複数編集は 1 回の LoadSong に coalesce される: {after_flush:?}"
    );

    // 追加編集なしで再 flush → epoch 一致で no-op (LoadSong 0)。 毎 frame 呼ばれても安全。
    app.flush_song_sync();
    let idle = drain(&mut audio_rx);
    assert_eq!(
        count_loadsong(&idle),
        0,
        "編集の無い flush は no-op: {idle:?}"
    );
}

/// (b) 編集直後に Play すると、 Play を送る前に最新 song を LoadSong で先に届ける
/// (ensure-synced): frame flush を待たずに engine が最新状態で再生を開始できる。
/// 追加編集の無い 2 回目 Play では LoadSong を再送しない (epoch 一致 = no-op)。
#[test]
fn play_after_edit_syncs_latest_song_before_play() {
    let (mut app, mut audio_rx, _plugin_rx, _proxy) = build_app();
    app.flush_song_sync();
    let _ = drain(&mut audio_rx);

    // 編集 (epoch bump) を frame flush 前に行い、 直後に Play。
    let _ = app.edit_song(|s| s.bpm = 142.0);
    app.handle_event(AppEvent::Play);

    let msgs = drain(&mut audio_rx);
    let load_idx = msgs
        .iter()
        .position(|m| matches!(m, AudioCommand::LoadSong(_)))
        .unwrap_or_else(|| panic!("編集後の Play は最新 song を LoadSong で先に送る: {msgs:?}"));
    let play_idx = msgs
        .iter()
        .position(|m| matches!(m, AudioCommand::Play))
        .unwrap_or_else(|| panic!("Play コマンドが送られる: {msgs:?}"));
    assert!(
        load_idx < play_idx,
        "LoadSong は Play より前に届く (ensure-synced): {msgs:?}"
    );
    // r.md #51: `transport.is_playing` は engine の観測値になったので、Play を
    // 送った直後には (Tick が来るまで) まだ立たない。「再生が始まったか」は上の
    // `AudioCommand::Play` 送信で直接見ている。

    // 追加編集の無い 2 回目 Play は LoadSong を再送しない (epoch 一致 = ensure-synced の
    // no-op)。 大量 WAV の再 compile 遅延を定常状態で踏まないための性質。
    app.handle_event(AppEvent::Stop);
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Play);
    let replay = drain(&mut audio_rx);
    assert_eq!(
        count_loadsong(&replay),
        0,
        "編集の無い Play は LoadSong を再送しない: {replay:?}"
    );
    assert!(
        replay.iter().any(|m| matches!(m, AudioCommand::Play)),
        "2 回目 Play も Play コマンドは送る: {replay:?}"
    );
}
