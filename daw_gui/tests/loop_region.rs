// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 再生ループ (ON/OFF + 範囲) を「dirty は立てないが保存される」 session state
//! (`common::model::LoopRegion`) へ移したことの回帰テスト。
//!
//! 検証する挙動:
//! - **dirty を立てない**: ループ範囲 / ON-OFF をどう変えても `*` (未保存マーク) が
//!   付かず、undo 履歴も汚さない (= ズーム / スクロールと同じ扱い)。
//! - **engine には届く**: それでも `AudioCommand::SetLoop` は毎回送られる
//!   (「保存しない」 のではなく「Song の編集ではない」 だけ)。
//! - **ripple 追従**: 時間の挿入 / 削除 (破壊的セクション編集) では、`Song` の外に
//!   住むループ範囲も clip / セクションと同じ規則でシフトする。
//!
//! save / load 往復と旧 `.daw` からの移行は `common::project` 側のテストが持つ
//! (`loop_region_roundtrips_through_view_state` /
//! `legacy_song_loop_range_migrates_to_loaded_loop_region`)。

use std::sync::Arc;

use common::model::{LoopRegion, Section};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
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

/// audio へ送られた `SetLoop` のうち最後のものを取り出す。
fn last_set_loop(rx: &mut UnboundedReceiver<AudioCommand>) -> Option<LoopRegion> {
    let mut last = None;
    while let Ok(cmd) = rx.try_recv() {
        if let AudioCommand::SetLoop(r) = cmd {
            last = Some(r);
        }
    }
    last
}

fn add_section(app: &mut AppData, id: u32, start: f64, len: f64) {
    app.edit_song(|song| {
        song.sections.push(Section {
            id,
            name: "A".to_string(),
            color: [0.3, 0.4, 0.5],
            start_beat: start,
            len_beats: len,
        });
    });
}

/// (a) ループ範囲 / ON-OFF の変更は dirty を立てず、undo 履歴も増やさない。
/// それでも engine には毎回 `SetLoop` が届く。
#[test]
fn loop_changes_do_not_dirty_the_project() {
    let (mut app, mut audio_rx, _p) = build_app();
    app.song_doc.mark_saved();
    assert!(!app.song_doc.is_dirty(), "前提: 保存直後は clean");
    let undo_depth = app.song_doc.undo_depth();

    app.handle_event(AppEvent::SetLoopRange { start: 4.0, end: 12.0 });
    assert!(!app.song_doc.is_dirty(), "ループ範囲の変更で '*' が付いてはいけない");
    assert_eq!(
        last_set_loop(&mut audio_rx),
        Some(LoopRegion { enabled: false, start_beat: 4.0, end_beat: 12.0 }),
        "dirty にしないが engine へは届く"
    );

    app.handle_event(AppEvent::ToggleLoop);
    assert!(!app.song_doc.is_dirty(), "ループ ON/OFF でも '*' が付いてはいけない");
    assert_eq!(
        last_set_loop(&mut audio_rx),
        Some(LoopRegion { enabled: true, start_beat: 4.0, end_beat: 12.0 })
    );

    assert_eq!(
        app.song_doc.undo_depth(),
        undo_depth,
        "undo 履歴も汚さない (ループは Song の編集ではない)"
    );
}

/// 範囲を「未定義」 に戻す指定 (end <= start) は 0/0 の正規形に畳まれる
/// (engine 側 `effective_loop_bounds` が曲全体へフォールバックする形)。
#[test]
fn degenerate_loop_range_collapses_to_undefined() {
    let (mut app, _a, _p) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 4.0, end: 12.0 });
    app.handle_event(AppEvent::SetLoopRange { start: 8.0, end: 8.0 });
    assert_eq!(app.transport.loop_region.range(), None);
    assert_eq!(app.transport.loop_region.start_beat, 0.0);
    assert_eq!(app.transport.loop_region.end_beat, 0.0);
}

/// (d) 時間の挿入 / 削除 (破壊的セクション編集の ripple) でループ範囲もシフトする。
/// `Song` から出た後もこの追従が消えないことが要求 (責務は `edit_song_rippling`)。
#[test]
fn ripple_from_section_edit_shifts_the_loop_range() {
    let (mut app, mut audio_rx, _p) = build_app();
    add_section(&mut app, 1, 0.0, 4.0);
    app.handle_event(AppEvent::SetLoopRange { start: 16.0, end: 20.0 });
    let _ = last_set_loop(&mut audio_rx);

    // [0,4) の帯を beat 20 へ移動 = 「[0,4) を詰める (close: from 4, -4)」 +
    // 「詰めた座標系の 16 に 4 拍空ける (open: from 16, +4)」。
    // ループ 16..20 は close で 12..16、open で 12..20 になる (start は open の
    // 起点より手前なので動かない = clip / セクションと同じ規則)。
    let changed = app.edit_song_rippling(|song| song.move_section(1, 20.0));
    assert!(changed, "セクションは実際に移動する");
    assert_eq!(
        (app.transport.loop_region.start_beat, app.transport.loop_region.end_beat),
        (12.0, 20.0),
        "ripple close + open がループ範囲にも同じ規則で効く"
    );

    // 帯は今 [16,20)。範囲ごと削除すると close(from 20, -4) だけが効き、
    // 20 以降だけが前へ詰まる (12 は不変、20 → 16)。
    let changed =
        app.edit_song_rippling(|song| song.delete_section_range(1).into_iter().collect());
    assert!(changed, "範囲削除は実際に起きる");
    assert_eq!(
        (app.transport.loop_region.start_beat, app.transport.loop_region.end_beat),
        (12.0, 16.0),
        "削除した 4 拍ぶんループ末尾も前へ詰まる"
    );
    assert_eq!(
        last_set_loop(&mut audio_rx),
        Some(app.transport.loop_region),
        "シフト後の範囲が engine にも届く"
    );
}
