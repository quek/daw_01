// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! app_state 配下の全サブモジュールが共有する fixture。
//! 旧 5 ファイルがほぼ verbatim 重複していたものの一本化。

use std::sync::Arc;

use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, PluginCommand, PluginEvent};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::select_modifier::SelectModifier;

fn entry(id: &str, name: &str, instrument: bool, path: &str) -> PluginEntry {
    PluginEntry {
        id: id.into(),
        format: PluginFormat::Clap,
        name: name.into(),
        vendor: "Test".into(),
        version: "1.0".into(),
        features: vec![if instrument { "instrument" } else { "audio-effect" }.into()],
        path: path.into(),
        descriptor_index: 0,
        has_note_input: instrument,
        has_note_output: false,
        has_audio_output: true,
        // instrument: audio を生成するだけ → 入力なし。effect: 加工する → 入力あり。
        has_audio_input: !instrument,
        has_video_input: false,
        has_video_output: false,
    }
}

/// テスト用 plugin_db。旧 5 ファイルが個別に持っていた entry の和集合
/// (synth / bitcrush / delay / fx)。各テストは id 指定 (`SelectPluginFromDb`)
/// で選ぶだけなので、使わない entry が余分に居ても不干渉 (entry 数や順序を
/// assert するテストはこの統合バイナリには無い)。`path` は実在不要
/// (production の plugin loader に通すわけではない)。
pub fn make_plugin_db() -> Arc<PluginDatabase> {
    Arc::new(PluginDatabase {
        entries: vec![
            entry("test.synth", "Test Synth", true, "C:/fake/synth.clap"),
            entry("test.bitcrush", "Test Bitcrush", false, "C:/fake/bitcrush.clap"),
            entry("test.delay", "Test Delay", false, "C:/fake/delay.clap"),
            entry("test.fx", "Test FX", false, "C:/fake/fx.clap"),
        ],
        scanned_at: None,
        port_probe_version: 0,
    })
}

/// AppData を test 用 dispatcher 込みで構築。dispatcher は trait 抽象に
/// なっているので winit EventLoop は不要。戻り値は最富形 — 使わない receiver
/// は呼び出し側 (各モジュールの thin adapter) が drop する。旧実装でも
/// audio_rx 即 drop (= closed channel) で全テストが通っており、AppData は
/// closed channel への send エラーを許容する。
pub fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,  // audio_rx
    UnboundedReceiver<PluginCommand>, // plugin_rx
    Arc<RecordingDispatcher>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher.clone();
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        Some(make_plugin_db()),
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, audio_rx, plugin_rx, event_dispatcher)
}

/// `rx` から現在キューにある全メッセージを取り出す。 試験 assertion 用。
pub fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

/// index 指定でトラックを 1 本だけ単独選択する (= production のトラックヘッダ
/// click 相当)。 production は可視順を `apply_select_tracks` に渡すので、 ここでも
/// 全トラックの id 列をそのまま可視順として渡す (折り畳み無しの test song では同じ)。
pub fn select_track_single(app: &mut AppData, idx: usize) {
    let visible: Vec<u32> = app.song_doc.song().tracks.iter().map(|t| t.id).collect();
    let id = visible[idx];
    app.apply_select_tracks(id, SelectModifier::Single, &visible);
}

/// 楽器 (test.synth) を track 0 に picker 経由でロードし、plugin_host からの
/// `SlotPluginLoaded` 応答まで fake dispatch する。
pub fn load_instrument(app: &mut AppData) {
    let track_id = app.song_doc.song().tracks[0].id;
    select_track_single(app, 0);
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    // 単一デバイスチェーン: picker は末尾 append、 空チェーンなので index 0。
    fake_plugin_loaded(app, track_id, 0, "test.synth");
}

/// ヘルパ: plugin_host の `SlotPluginLoaded` を AppEvent として fake
/// dispatch。 production で plugin_host が返す内容を test がそのまま模倣する。
/// `index` は flat な device index (= 末尾 append した位置)。
///
/// v29: 応答は安定 `device_id` + 要求 `generation` の echo。 song の device
/// から id を解決し、 直前の `SetSlotPlugin` 送信で AppData が登録した
/// pending generation をそのまま返す (= production の echo と同じ)。
/// 戻り値は device_id (以後の `ClosePluginShmem` 等の assert 用)。
pub fn fake_plugin_loaded(app: &mut AppData, track_id: u32, index: u32, id: &str) -> u64 {
    let device_id = daw_gui::app::device_id_at(app.song_doc.song(), track_id, index)
        .expect("fake_plugin_loaded: no device at (track_id, index)");
    let generation = app
        .ipc.pending_plugin_loads
        .get(&device_id)
        .copied()
        .expect("fake_plugin_loaded: SetSlotPlugin was not pending for this device");
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoaded {
        device_id,
        id: id.into(),
        name: id.into(),
        shmem_id: String::new(),
        // テストは state 復元 path をシミュレートしない (= initial_state =
        // None でロードしたのと等価)。
        state_load_error: None,
        aux_output_count: 0,
        generation,
    }));
    device_id
}
