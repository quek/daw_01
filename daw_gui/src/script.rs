//! JavaScript scripting host (boa_engine) — production binary を CLI
//! `daw_gui --script <file.js>` で headless に駆動するためのモジュール。
//!
//! reference DAW (Ardour Lua / Reaper ReaScript) の flat namespace 流に
//! `daw.*` で API を露出。 同期モデル — 各 method が IPC を送って完了
//! event を `pump_until` で blocking 受信する。 boa の async runtime は
//! 使わない (低頻度 orchestration には不要)。
//!
//! boa は native function 内で captures を持つには `boa_gc::Trace` 実装が
//! 必要なので、 `ScriptHost` は thread_local に置いて native function は
//! 1 つの session で 1 thread からしか触らない (script は単一実行)。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
mod editing;
mod host;
mod native_device;
mod tabs;

pub use host::{ScriptAttach, ScriptIo, ScriptStarter, prepare_gui, run_scripted};
use host::{HOST, ScriptHost, device_id_at_app, with_app, with_host};

use boa_engine::native_function::NativeFunctionPointer;
use boa_engine::property::Attribute;
use boa_engine::value::TryFromJs;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, NativeFunction,
    js_string,
};
use common::model::Song;
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, AudioEvent, DeviceAddr, PluginCommand, PluginEvent};

use crate::app::{AppEvent, ClipKey};
use crate::bootstrap::ChildEvent;

// ---------------------------------------------------------------------------
// `daw.*` global の登録
// ---------------------------------------------------------------------------

/// `daw.*` に並べる関数の表 `(JS 名, 実装, 引数の数)`。**binding はここ 1 か所に並べる**
/// (どの API があるかを 1 画面で見渡せるように)。実装は分野ごとに `script/*.rs` へ置いてよい。
const DAW_API: &[(&str, NativeFunctionPointer, usize)] = &[
    ("loadSongFromObject", daw_load_song_from_object, 1),
    ("setSlotPlugin", daw_set_slot_plugin, 6),
    ("waitForPluginLoaded", daw_wait_for_plugin_loaded, 4),
    ("setGeneratedAudio", daw_set_generated_audio, 3),
    ("exportWav", daw_export_wav, 2),
    // ----- headless export-range test harness ----------------
    ("loadSongFile", daw_load_song_file, 1),
    ("log", daw_log, 1),
    ("perfJson", tabs::daw_perf_json, 0),
    // ----- GUI と同じ経路 (人の操作と区別が付かない口。`--gui` での再現・計測に使う) -----
    ("appOpenProject", daw_app_open_project, 1),
    ("appPlay", daw_app_play, 0),
    ("appStop", daw_app_stop, 0),
    ("play", daw_play, 0),
    ("stop", daw_stop, 0),
    ("startRecording", daw_start_recording, 1),
    ("stopRecording", daw_stop_recording, 0),
    ("transportState", daw_transport_state, 0),
    ("sleepMs", daw_sleep_ms, 1),
    ("reinitForExport", daw_reinit_for_export, 1),
    ("exportWavRange", daw_export_wav_range, 4),
    ("analyzeLoudnessJson", daw_analyze_loudness, 3),
    ("setDeviceLatency", daw_set_device_latency, 2),
    ("takePluginLoadEventsJson", daw_take_plugin_load_events, 0),
    // ----- `docs/plan_project_tabs.md` §5.4: プロジェクトタブ -----
    ("newTab", tabs::daw_new_tab, 0),
    ("closeTab", tabs::daw_close_tab, 0),
    ("switchTab", tabs::daw_switch_tab, 1),
    ("tabsJson", tabs::daw_tabs_json, 0),
    ("metricsJson", tabs::daw_metrics_json, 0),
    ("pendingPluginLoadsJson", daw_pending_plugin_loads, 0),
    // ----- PR7 follow-up (JS test infra) ----------------------------
    // app.* の API は ScriptHost::app (= AppData) を直接 mutate して
    // production と同じ AppEvent handler を回す。 IPC は AppData の
    // 内部 send_audio / send_plugin から bootstrap の channel に
    // 流れる。 production GUI mode と挙動を一致させる。
    ("appLoadSongJson", daw_app_load_song_json, 1),
    ("inspectSongJson", daw_inspect_song_json, 0),
    ("clipDisplayLabel", daw_clip_display_label, 1),
    ("deviceChain", daw_device_chain, 1),
    ("relocateDevices", daw_relocate_devices, 5),
    ("setSelection", daw_set_selection, 1),
    ("setTimeSelection", daw_set_time_selection, 3),
    ("duplicateTracks", daw_duplicate_tracks, 2),
    ("setHoverClip", daw_set_hover_clip, 1),
    ("setHoverBeat", daw_set_hover_beat, 1),
    ("dispatchSplit", daw_dispatch_split, 1),
    ("dispatchGlue", daw_dispatch_glue, 0),
    ("dispatchBounceWithFx", daw_dispatch_bounce_with_fx, 1),
    ("dispatchRenameClip", daw_dispatch_rename_clip, 2),
    // ----- Phase 7 B5 Scale & Root API ------------------------------
    ("setScaleAtPlayhead", editing::daw_set_scale_at_playhead, 2),
    ("clearScaleChanges", editing::daw_clear_scale_changes, 0),
    ("toggleSnapOnDraw", editing::daw_toggle_snap_on_draw, 0),
    ("toggleSnapLiveInput", editing::daw_toggle_snap_live_input, 0),
    ("toggleVirtualKeyboard", editing::daw_toggle_virtual_keyboard, 0),
    ("virtualKeyboardKey", editing::daw_virtual_keyboard_key, 3),
    ("toggleFoldToScale", editing::daw_toggle_fold_to_scale, 0),
    ("quantizePitchesToScale", editing::daw_quantize_pitches_to_scale, 1),
    ("addNote", editing::daw_add_note, 5),
    ("setNotePositionsJson", editing::daw_set_note_positions_json, 1),
    // ----- r.md #129 内蔵 device (`docs/plan_rack_native_devices.md` §10.19) -----
    ("nativeDevices", native_device::daw_native_devices, 1),
    ("nativeEdit", native_device::daw_native_edit, 2),
    ("nativeGainReduction", native_device::daw_native_gain_reduction, 1),
    ("masterLimiterGainReduction", native_device::daw_master_limiter_gain_reduction, 0),
    ("setDevicesBypassed", native_device::daw_set_devices_bypassed, 2),
    ("setScListen", native_device::daw_set_sc_listen, 1),
    ("masterPeakDbfs", native_device::daw_master_peak_dbfs, 1),
];

fn register_daw_globals(ctx: &mut Context) -> Result<()> {
    let mut init = boa_engine::object::ObjectInitializer::new(ctx);
    for &(name, function, length) in DAW_API {
        // `daw.*` の 1 呼び出しを GUI の 1 frame とみなし、抜けたところで frame 末と同じ子プロセス sync を回す
        // (`runner.rs` の `flush_all_song_sync`)。headless には frame loop が無いので、ここで回さないと
        // handler が Song を編集しても daw_audio / plugin host に届かない。epoch が進んでいなければ no-op。
        let frame = move |this: &JsValue, args: &[JsValue], ctx: &mut Context| {
            let result = function(this, args, ctx);
            HOST.with_borrow_mut(|h| {
                if let Some(host) = h.as_mut() {
                    host.flush_after_call();
                }
            });
            result
        };
        init.function(NativeFunction::from_copy_closure(frame), JsString::from(name), length);
    }
    let daw = init.build();

    // `daw.scriptArgs` = { output: <CLI で指定された --output or null>,
    //                      <extra key>: <extra value>, ... }
    let args_obj = boa_engine::object::ObjectInitializer::new(ctx).build();
    let (output_value, extras): (JsValue, Vec<(String, String)>) =
        HOST.with_borrow(|h| {
            let host = h.as_ref();
            let output = match host.and_then(|h| h.script_args.output.as_ref()) {
                Some(p) => JsString::from(p.to_string_lossy().as_ref()).into(),
                None => JsValue::null(),
            };
            let extras = host
                .map(|h| h.script_args.extra.clone())
                .unwrap_or_default();
            (output, extras)
        });
    args_obj
        .set(js_string!("output"), output_value, false, ctx)
        .map_err(|e| anyhow!("set scriptArgs.output: {e}"))?;
    for (k, v) in extras {
        let key = JsString::from(k.as_str());
        let val: JsValue = JsString::from(v.as_str()).into();
        args_obj
            .set(key, val, false, ctx)
            .map_err(|e| anyhow!("set scriptArgs.{k}: {e}"))?;
    }
    daw.set(js_string!("scriptArgs"), args_obj, false, ctx)
        .map_err(|e| anyhow!("set daw.scriptArgs: {e}"))?;

    ctx.register_global_property(js_string!("daw"), daw, Attribute::all())
        .map_err(|e| anyhow!("register daw global: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 個々の API 実装 (NativeFunction::from_fn_ptr に渡す signature: fn(&JsValue,
// &[JsValue], &mut Context) -> JsResult<JsValue>)
// ---------------------------------------------------------------------------

fn daw_load_song_from_object(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let obj = args.get_or_undefined(0);
    // JSON.stringify(obj) → serde_json::from_str → Song
    let json_global_value = ctx.global_object().get(js_string!("JSON"), ctx)?;
    let json_global = json_global_value
        .as_object()
        .ok_or_else(|| JsNativeError::error().with_message("JSON global not found"))?;
    let stringify_value = json_global.get(js_string!("stringify"), ctx)?;
    let stringify = stringify_value
        .as_object()
        .ok_or_else(|| JsNativeError::error().with_message("JSON.stringify not found"))?;
    let json = stringify.call(&JsValue::from(json_global), std::slice::from_ref(obj), ctx)?;
    let json_str = json
        .as_string()
        .ok_or_else(|| JsNativeError::error().with_message("JSON.stringify returned non-string"))?
        .to_std_string_escaped();

    let mut value: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
        JsError::from_native(JsNativeError::error().with_message(format!("song JSON parse: {e}")))
    })?;
    // (§10) 旧構造 (sidechain / 3-split chain / per-clip name+notes) を現行へ移してから untagged
    // clip_contents に type タグを注入し deserialize (ファイル load 経路 migrate_legacy_song と同じ)。
    common::project::migrate_legacy_song(&mut value);
    common::project::tag_clip_contents_in_song(&mut value);
    let mut song: Song = serde_json::from_value(value).map_err(|e| {
        JsError::from_native(JsNativeError::error().with_message(format!("song deserialize: {e}")))
    })?;
    // v29: 安定 id (track / device / send / note...) を採番してから流す。
    // device_id addressing (`daw.setSlotPlugin` 等) がこの id を引く。
    song.ensure_ids();
    with_app(move |app, _side, _io| {
        // 曲の持ち主は app の `song_doc` 1 つ (GUI の Open と同じ)。engine へは呼び出しを抜けたところの frame 境界で
        // 送られる — ここで LoadSong を直接送ると、次の frame 境界が app 側の (古い) 曲で上書きする。
        app.cur.song_doc.replace_song(song);
        app.after_song_replaced();
    });
    Ok(JsValue::undefined())
}

fn daw_set_slot_plugin(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // 単一デバイスチェーン: `daw.setSlotPlugin(track, index, format, path, id)`。
    // 旧 (slot_kind, slot_index) 2 引数を flat な device `index` 1 つに統合。
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let index = u32::try_from_js(args.get_or_undefined(1), ctx)?;
    let format_str = String::try_from_js(args.get_or_undefined(2), ctx)?;
    let path_str = String::try_from_js(args.get_or_undefined(3), ctx)?;
    let plugin_id = String::try_from_js(args.get_or_undefined(4), ctx)?;

    let format = match format_str.as_str() {
        "clap" => PluginFormat::Clap,
        "vst3" => PluginFormat::Vst3,
        s => {
            return Err(JsNativeError::range()
                .with_message(format!("invalid format {s:?}; expected 'clap' or 'vst3'"))
                .into());
        }
    };
    // closure は event loop スレッドで走り得るので Send な値 (String) で返し、JsError には
    // script スレッドで包む。
    with_app(move |app, side, io| -> Result<(), String> {
        // v29: 安定 device id でアドレスする。 事前に loadSongFromObject /
        // loadSongFile 済みの song から解決する (ensure_ids 済み)。
        let Some(device_id) = device_id_at_app(app, track_id, index) else {
            return Err(format!(
                "setSlotPlugin: no device at (track {track_id}, index {index}) — \
                 load a song with that device first (v29 requires stable device ids)"
            ));
        };
        // generation を採番して app の pending にも登録 (= 応答の echo が
        // AppData の世代 guard を通り、 OpenPluginShmem forward まで走る)。
        side.next_raw_load_generation = side.next_raw_load_generation.wrapping_add(1).max(1);
        let generation = side.next_raw_load_generation;
        app.cur.pipc.pending_plugin_loads.insert(device_id, generation);
        let _ = io.plugin_tx.send(PluginCommand::SetSlotPlugin {
            device: app.dev(device_id),
            format,
            path: PathBuf::from(path_str),
            plugin_id,
            initial_state: None,
            generation,
        });
        Ok(())
    })
    .map_err(js_native)?;
    Ok(JsValue::undefined())
}

fn daw_wait_for_plugin_loaded(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // 単一デバイスチェーン: `daw.waitForPluginLoaded(track, index, timeout)`。
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let want_index = u32::try_from_js(args.get_or_undefined(1), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(2), ctx).unwrap_or(30_000);

    // v29: 応答は device_id keyed。 期待 device を先に解決 (app の手) してから待つ (script スレッド)。
    let want_device = with_app(move |app, _, _| device_id_at_app(app, track_id, want_index));
    let res = with_host(|h| -> Result<ChildEvent> {
        let want_device = want_device.ok_or_else(|| {
            anyhow!("waitForPluginLoaded: no device at (track {track_id}, index {want_index})")
        })?;
        h.pump_until(
            |msg| {
                matches!(
                    msg,
                    ChildEvent::Plugin(PluginEvent::SlotPluginLoaded { device: DeviceAddr { device_id, .. }, .. })
                        if *device_id == want_device
                )
            },
            Duration::from_millis(timeout_ms),
        )
    });
    res.map_err(|e| {
        JsError::from_native(
            JsNativeError::error().with_message(format!("waitForPluginLoaded: {e}")),
        )
    })?;
    Ok(JsValue::undefined())
}

/// PR-V4: JS test API `daw.setGeneratedAudio` は無効化 (= no-op)。 旧
/// 旧 `SetGeneratedAudio` IPC 経路を IPC から削除したため。
/// 新しい builtin VOICEVOX 経路では plugin が自前で synth するので、
/// JS 側から audio buffer を直接注入する API は不要。
fn daw_set_generated_audio(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    tracing::warn!(
        "daw.setGeneratedAudio: PR-V4 で削除済 (= builtin VOICEVOX plugin 経由に移行)、 no-op"
    );
    Ok(JsValue::undefined())
}

fn daw_export_wav(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let path_str = String::try_from_js(args.get_or_undefined(0), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(1), ctx).unwrap_or(60_000);

    let project = with_app(|app, _, _| app.pk());
    let pump_result = with_host(|h| {
        // PR3.3: 直前に発火された IPC events (PluginLatencyChanged 等) を
        // exportWav の前に drain して、 latency が `compile_schedule` まで反映されるのを待つ
        // (drain した event による Song の編集は `handle_incoming` が flush する)。
        // export thread は ExportWav arrival 時点で `shared.song` を snapshot
        // するので、 event drain して LoadSong を先に届けないと PDC が
        // 適用されない song で render が始まる。
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.io.audio_tx.send(AudioCommand::ExportWav {
            project,
            path: PathBuf::from(path_str),
            // scripting API は全曲 export (レンジ指定は GUI 専用)。
            range: None,
            // standalone WAV (video render なし) なので modulation sidecar は不要。
            write_mod_sidecar: false,
        });
        h.pump_until(
            |msg| matches!(msg, ChildEvent::Audio(AudioEvent::ExportWavComplete { .. })),
            Duration::from_millis(timeout_ms),
        )
    });
    match pump_result {
        Ok(ChildEvent::Audio(AudioEvent::ExportWavComplete { error: None, .. })) => {
            Ok(JsValue::undefined())
        }
        Ok(ChildEvent::Audio(AudioEvent::ExportWavComplete { error: Some(e), .. })) => {
            Err(JsError::from_native(
                JsNativeError::error().with_message(format!("export failed: {e}")),
            ))
        }
        Ok(_) => unreachable!(),
        Err(e) => Err(JsError::from_native(
            JsNativeError::error().with_message(format!("exportWav: {e}")),
        )),
    }
}

/// `daw.loadSongFile(path)` — open a real `.daw` project headlessly, exactly
/// like the GUI's File→Open: deserialize, populate the plugin DB, instantiate
/// every plugin (`SetSlotPlugin`), and push the song + project_dir to the audio
/// engine. Lets an automated test drive the *real* project (real plugins) with
/// no human operating the GUI (export-bleed regression harness).
fn daw_load_song_file(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path_str = String::try_from_js(args.get_or_undefined(0), ctx)?;
    let path = PathBuf::from(&path_str);
    let mut song = common::project::load(&path)
        .map_err(|e| js_native(format!("loadSongFile: load {path_str}: {e}")))?;
    song.ensure_ids();
    with_app(move |app, _side, io| {
        // Resolve plugin ids → DLL paths from the cached DB the bootstrap built.
        app.ipc.plugin_db = io.plugin_db.clone();
        app.cur.song_doc.file_path = Some(path.clone());
        // GUI の File→Open と同じ順序を厳守する。 teardown を飛ばすと前 song の
        // plugin instance が plugin_host に残り、 device_id が project ごとに
        // 再採番されるせいで新 song の SetSlotPlugin が **旧 instance に dedup
        // 吸収** される (= 保存した音色が復元されず前 project の音で鳴る)。
        // 2 本目以降の loadSongFile を呼ぶ headless テストでは race なしに毎回
        // 発生するので、 テストが緑でも実機と一致しなくなる。
        app.teardown_all_loaded_plugins();
        // Instantiate every plugin in the chain (sends SetSlotPlugin), then push
        // the song + project_dir + LoadSong to the audio engine.
        app.restore_plugin_from_song(&song);
        app.cur.song_doc.replace_song(song.clone());
        // Song スコープの派生状態を破棄する唯一の口 (GUI 経路と同じ)。LoadSong は呼び出しを抜けたところで
        // 送られる (frame 境界、replace_song が epoch を bump している)。
        app.after_song_replaced();
    });
    Ok(JsValue::undefined())
}

/// `daw.log(msg)` — 計測値などを stdout と tracing (INFO) の両方へ。boa には `console` が無い。
fn daw_log(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let msg = arg_to_string(args, 0, ctx)?;
    println!("{msg}");
    tracing::info!(target: "daw_gui::script::js", "{msg}");
    Ok(JsValue::undefined())
}

/// `daw.appOpenProject(path)` — File→Open / Open Recent と **同じ経路** (`AppEvent::OpenRecent`
/// → `open_path_in_tab`) でプロジェクトを開く。`loadSongFile` が script 専用の最短経路なのに
///対し、こちらはタブ・recent・recovery・asset decode まで人の操作と同じ副作用が起きる。
/// `--gui` で「実機で開いたときに何が起きるか」を再現・計測するための口。
fn daw_app_open_project(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = PathBuf::from(String::try_from_js(args.get_or_undefined(0), ctx)?);
    with_app(move |app, _side, _io| app.handle_event(AppEvent::OpenRecent(path)));
    Ok(JsValue::undefined())
}

/// `daw.appPlay()` — Space と同じ経路 (`AppEvent::Play`: 再生ゲート / count-in / ランチャーの
/// seed を含む)。engine へ直接 `Play` を送る `daw.play()` と使い分ける。
fn daw_app_play(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::Play));
    Ok(JsValue::undefined())
}

/// `daw.appStop()` — `AppEvent::Stop` (GUI の停止と同じ経路)。
fn daw_app_stop(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(|app, _side, _io| app.handle_event(AppEvent::Stop));
    Ok(JsValue::undefined())
}

/// `daw.play()` — start realtime transport in the audio engine (bypasses the
/// GUI play-gating, which assumes the winit loop). Used to reproduce the
/// "played first" state where a synth holds a live voice.
fn daw_play(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(move |app, _side, io| {
        let _ = io.audio_tx.send(AudioCommand::Play { project: app.pk() });
    });
    Ok(JsValue::undefined())
}

/// `daw.stop()` — stop realtime transport.
fn daw_stop(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(move |app, _side, io| {
        let _ = io.audio_tx.send(AudioCommand::Stop { project: app.pk() });
    });
    Ok(JsValue::undefined())
}

/// `daw.startRecording(prerollSamples)` — open a recording session in the audio
/// engine (r.md #51). Mirrors what the GUI sends when Rec is pressed, so the
/// headless harness can exercise the engine-side contract (count-in, song-end
/// auto-stop suppression, `recording_live` publication).
fn daw_start_recording(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let preroll_samples = u64::try_from_js(args.get_or_undefined(0), ctx).unwrap_or(0);
    with_app(move |app, _side, io| {
        let _ = io.audio_tx
            .send(AudioCommand::StartRecording { project: app.pk(), preroll_samples });
    });
    Ok(JsValue::undefined())
}

/// `daw.stopRecording()` — close the engine-side recording session (r.md #51).
/// Does not stop the transport (punch-out keeps playing).
fn daw_stop_recording(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_app(move |app, _side, io| {
        let _ = io.audio_tx.send(AudioCommand::StopRecording { project: app.pk() });
    });
    Ok(JsValue::undefined())
}

/// `daw.transportState()` — read what the audio engine is publishing about the
/// transport (r.md #51): `{ playing, recordingLive, prerollRemaining, playhead }`.
///
/// This is the same shmem plane the GUI's playhead poller reads, so a headless
/// script can verify the engine half of "who owns the transport state" without a
/// window (`feedback_prefer_headless_verification`).
fn daw_transport_state(_this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (playing, recording_live, preroll, playhead) = with_app(move |app, _side, io| {
        // `docs/plan_project_tabs.md`: アクティブなタブの telemetry slot を読む
        // (まだ claim されていなければ停止状態として返す)。
        match io.bridge.find_project(app.pk()) {
            Some(b) => (b.playing(), b.recording_live(), b.preroll_remaining(), b.playhead_samples()),
            None => (false, false, 0, 0),
        }
    });
    let obj = JsObject::default(ctx.intrinsics());
    obj.set(js_string!("playing"), playing, false, ctx)?;
    obj.set(js_string!("recordingLive"), recording_live, false, ctx)?;
    obj.set(js_string!("prerollRemaining"), preroll as f64, false, ctx)?;
    obj.set(js_string!("playhead"), playhead as f64, false, ctx)?;
    Ok(obj.into())
}

/// `daw.sleepMs(ms)` — wall-clock wait that keeps pumping incoming IPC events
/// (so plugin loads / shmem opens are serviced). Lets realtime playback run for
/// a fixed duration and gives async plugin instantiation time to complete.
fn daw_sleep_ms(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = u64::try_from_js(args.get_or_undefined(0), ctx).unwrap_or(0);
    with_host(|h| h.drain_pending_for(Duration::from_millis(ms)));
    Ok(JsValue::undefined())
}

/// `daw.reinitForExport(timeoutMs)` — reinitialise all plugins (deactivate→
/// activate) for a clean offline cold render and block until done. Mirrors the
/// GUI's pre-export reinit step so the headless harness exercises the real fix.
fn daw_reinit_for_export(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let timeout_ms = u64::try_from_js(args.get_or_undefined(0), ctx).unwrap_or(30_000);
    let project = with_app(|app, _, _| app.pk());
    let res = with_host(|h| {
        let _ = h.io.plugin_tx.send(PluginCommand::ReinitAllPlugins { project: Some(project) });
        h.pump_until(
            |msg| matches!(msg, ChildEvent::Plugin(PluginEvent::PluginsReinitDone { .. })),
            Duration::from_millis(timeout_ms),
        )
    });
    res.map_err(|e| js_native(format!("reinitForExport: {e}")))?;
    Ok(JsValue::undefined())
}

/// `daw.exportWavRange(path, startBeat, endBeat, timeoutMs)` — offline export
/// of a beat range (the cold range, GUI's
/// `AudioCommand::ExportWav { project: h.app.pk(), range: Some(..) }`), driven headlessly.
///
/// r.md #54: 引数は **拍** (旧: サンプルフレーム)。拍→サンプル換算は daw_audio 側
/// (`beats_to_samples` = tempo automation を積分する SSoT) 一本になった。
fn daw_export_wav_range(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path_str = String::try_from_js(args.get_or_undefined(0), ctx)?;
    let start = f64::try_from_js(args.get_or_undefined(1), ctx)?;
    let end = f64::try_from_js(args.get_or_undefined(2), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(3), ctx).unwrap_or(120_000);
    let project = with_app(|app, _, _| app.pk());
    let pump_result = with_host(|h| {
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.io.audio_tx.send(AudioCommand::ExportWav {
            project,
            path: PathBuf::from(path_str),
            range: Some((start, end)),
            write_mod_sidecar: false,
        });
        h.pump_until(
            |msg| matches!(msg, ChildEvent::Audio(AudioEvent::ExportWavComplete { .. })),
            Duration::from_millis(timeout_ms),
        )
    });
    match pump_result {
        Ok(ChildEvent::Audio(AudioEvent::ExportWavComplete { error: None, .. })) => {
            Ok(JsValue::undefined())
        }
        Ok(ChildEvent::Audio(AudioEvent::ExportWavComplete { error: Some(e), .. })) => {
            Err(js_native(format!("exportWavRange failed: {e}")))
        }
        Ok(_) => unreachable!(),
        Err(e) => Err(js_native(format!("exportWavRange: {e}"))),
    }
}

/// `daw.analyzeLoudnessJson(startBeat, endBeat, timeoutMs)` — r.md #54 の範囲
/// ラウドネス解析を headless で走らせ、確定レポートを JSON 文字列で返す。
///
/// `startBeat >= endBeat` を渡すと全曲 (`range = None`)。GUI と違って
/// プラグイン再初期化は挟まないので、必要なら先に `daw.reinitForExport()` を呼ぶ
/// (`exportWavRange` と同じ流儀)。
fn daw_analyze_loudness(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let start = f64::try_from_js(args.get_or_undefined(0), ctx)?;
    let end = f64::try_from_js(args.get_or_undefined(1), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(2), ctx).unwrap_or(120_000);
    let range = (end > start).then_some((start, end));
    let project = with_app(|app, _, _| app.pk());
    let pump_result = with_host(|h| {
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.io.audio_tx.send(AudioCommand::AnalyzeLoudness { project, range });
        h.pump_until(
            |msg| {
                matches!(
                    msg,
                    ChildEvent::Audio(AudioEvent::LoudnessAnalysisComplete { .. })
                )
            },
            Duration::from_millis(timeout_ms),
        )
    });
    match pump_result {
        Ok(ChildEvent::Audio(AudioEvent::LoudnessAnalysisComplete {
            report: Some(r),
            error: None,
            cancelled: false,
            ..
        })) => {
            // 曲線とヒストグラムは巨大なので、スカラーだけを JSON にする
            // (headless の検証で欲しいのは数値)。
            let json = format!(
                concat!(
                    "{{\"integrated_lufs\":{},\"lra_lu\":{},\"max_momentary_lufs\":{},",
                    "\"max_short_term_lufs\":{},\"true_peak_dbtp\":{},\"sample_peak_dbfs\":{},",
                    "\"clipped_samples\":{},\"measured_secs\":{},\"total_frames\":{},",
                    "\"sample_rate\":{}}}"
                ),
                json_f32(r.integrated_lufs),
                json_f32(r.lra_lu),
                json_f32(r.max_momentary_lufs),
                json_f32(r.max_short_term_lufs),
                json_f32(r.true_peak_dbtp),
                json_f32(r.sample_peak_dbfs),
                r.clipped_samples,
                json_f32(r.measured_secs),
                r.total_frames,
                r.sample_rate,
            );
            Ok(JsValue::from(js_string!(json.as_str())))
        }
        Ok(ChildEvent::Audio(AudioEvent::LoudnessAnalysisComplete { error: Some(e), .. })) => {
            Err(js_native(format!("analyzeLoudness failed: {e}")))
        }
        Ok(ChildEvent::Audio(AudioEvent::LoudnessAnalysisComplete { cancelled: true, .. })) => {
            Err(js_native("analyzeLoudness cancelled".to_string()))
        }
        // `report: None` + `error: None` は daw_audio が出さない形。panic せず
        // エラーで返す (headless driver を落とすより診断可能)。
        Ok(other) => Err(js_native(format!("analyzeLoudness: unexpected {other:?}"))),
        Err(e) => Err(js_native(format!("analyzeLoudness: {e}"))),
    }
}

/// `-inf` / `NaN` は JSON に書けないので `null` にする。
fn json_f32(v: f32) -> String {
    if v.is_finite() { format!("{v:.4}") } else { "null".to_string() }
}

/// `daw.setDeviceLatency(deviceId, samples)` — plugin host の latency 報告を
/// 手で注入する (実プラグイン無しで PDC を検証する headless テスト用)。
///
/// 旧 `setTrackLatency` は `Song` の track 合計フィールドを書き換えていたが、
/// 報告 latency は `Song` から外れた (r.md #9) ので device 単位の中継に置き換えた。
/// production の `PluginEvent::PluginLatencyChanged` 経路と同じコマンドを送る。
fn daw_set_device_latency(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let device_id = u64::try_from_js(args.get_or_undefined(0), ctx)?;
    let samples = u32::try_from_js(args.get_or_undefined(1), ctx)?;
    with_app(move |app, _side, io| {
        let _ = io.audio_tx
            .send(AudioCommand::SetDeviceLatency { project: app.pk(), device_id, samples });
    });
    Ok(JsValue::undefined())
}

/// `daw.takePluginLoadEventsJson()` — 直近の呼び出し以降に観測した plugin load
/// 応答を `{"loaded":[device_id...],"failed":[{device_id,plugin_id,reason}...]}`
/// で返し、 バッファを空にする。
///
/// plugin load の成否を **ログ grep ではなく script 内の assertion** で判定する
/// ための hook。 「同じプロジェクトを連続で開いても全 device が load できる」
/// (shmem 名の再利用レース回帰) を headless で固定するのに使う。
fn daw_take_plugin_load_events(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = with_app(move |_app, side, _io| {
        let events = std::mem::take(&mut side.plugin_load_events);
        serde_json::to_string(&events)
    })
    .map_err(|e| js_native(format!("takePluginLoadEventsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.pendingPluginLoadsJson()` — `SetSlotPlugin` を送ったが応答
/// (`SlotPluginLoaded` / `SlotPluginLoadFailed`) がまだ来ていない device id の
/// 配列。 `loadSongFile` 直後は「この project で load を要求した device 全部」、
/// 全応答が揃った後は空になる。 期待値を script 側で二重管理せずに済む。
fn daw_pending_plugin_loads(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = with_app(move |app, _side, _io| {
        let mut ids: Vec<u64> = app.cur.pipc.pending_plugin_loads.keys().copied().collect();
        ids.sort_unstable();
        serde_json::to_string(&ids)
    })
    .map_err(|e| js_native(format!("pendingPluginLoadsJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

// ---------------------------------------------------------------------------
// PR7 follow-up: AppData-driven test API (`daw.appLoadSongJson` /
// `inspectSongJson` / `setSelection` / `setHoverClip` / `setHoverBeat` /
// `dispatchSplit` / `dispatchGlue`).
//
// 全 JS ↔ Rust の橋は **JSON 文字列** で統一して boa の object iteration
// 沼を避ける。 JS 側は `JSON.stringify` / `JSON.parse` を使うだけ。 全 API
// は同期 (= AppData の handler を直接呼ぶ)、 IPC は AppData 内部の
// `send_audio` / `send_plugin` から bootstrap channel に流れる。
// ---------------------------------------------------------------------------

fn js_native(msg: impl Into<String>) -> JsError {
    JsError::from_native(JsNativeError::error().with_message(msg.into()))
}

fn arg_to_string(args: &[JsValue], idx: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args
        .get_or_undefined(idx)
        .to_string(ctx)?
        .to_std_string_escaped())
}

fn daw_app_load_song_json(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let mut value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| js_native(format!("appLoadSongJson: parse: {e}")))?;
    // (§10) 旧構造 (sidechain / 3-split chain / per-clip name+notes) を現行へ移してから untagged
    // clip_contents に `type` タグを注入し deserialize (ファイル load 経路 migrate_legacy_song +
    // migrate_clip_content_add_tag と同じ前処理)。
    common::project::migrate_legacy_song(&mut value);
    common::project::tag_clip_contents_in_song(&mut value);
    let mut song: Song = serde_json::from_value(value)
        .map_err(|e| js_native(format!("appLoadSongJson: deserialize: {e}")))?;
    song.ensure_ids();
    song.ensure_clip_contents();
    song.ensure_audio_source_ids();
    with_app(move |app, _side, _io| app.cur.song_doc.replace_song(song));
    Ok(JsValue::undefined())
}

fn daw_inspect_song_json(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    // v20+ clip names live in the SSoT `Song.clip_content_names`, not in
    // `Clip.name` (drained to empty on load, `skip_serializing_if`). Serialize
    // a clone with names re-hydrated from the SSoT so the inspected JSON
    // reflects what the user actually sees (and matches the rename smoke
    // test's contract). Without this, every clip name reads back `undefined`.
    let json = with_app(move |app, _side, _io| {
        let song = app.cur.song_doc.song();
        let names = &song.clip_content_names;
        // §10 で `Clip.name` field を撤去。per-clip 名の SSoT は `clip_content_names` (map)。
        // inspection JSON では serialize 後の各 clip オブジェクトへ `name` を注入し
        // 「ユーザーに見える名前」を反映する (rename smoke test の contract 用)。
        let mut value = serde_json::to_value(song)?;
        fn inject_names(
            clips: Option<&mut serde_json::Value>,
            names: &std::collections::HashMap<common::model::ContentId, String>,
        ) {
            let Some(arr) = clips.and_then(serde_json::Value::as_array_mut) else {
                return;
            };
            for clip in arr {
                let Some(cid) = clip.get("content_id").and_then(serde_json::Value::as_u64) else {
                    continue;
                };
                if let Some(n) = names.get(&(cid as common::model::ContentId)) {
                    clip["name"] = serde_json::Value::String(n.clone());
                }
            }
        }
        if let Some(tracks) = value.get_mut("tracks").and_then(serde_json::Value::as_array_mut) {
            for track in tracks {
                inject_names(track.get_mut("clips"), names);
                if let Some(lanes) = track
                    .get_mut("automation_lanes")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for lane in lanes {
                        inject_names(lane.get_mut("clips"), names);
                    }
                }
            }
        }
        if let Some(lanes) = value
            .get_mut("song_lanes")
            .and_then(serde_json::Value::as_array_mut)
        {
            for lane in lanes {
                inject_names(lane.get_mut("clips"), names);
            }
        }
        serde_json::to_string(&value)
    })
    .map_err(|e| js_native(format!("inspectSongJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.clipDisplayLabel(refJson)` — return the **rendered** clip label
/// (`clip_display_label` の結果) for `{track, clip}` indices. `inspectSongJson`
/// が返す `content_name` (= モデルの明示名) と違い、 Text 本文 / 歌詞 / 明示名の
/// 導出後の **画面に出る文字列** を返す。 (歌詞付きクリップを rename
/// しても歌詞のまま) の回帰を headless で検証するための hook。 存在しない
/// track / clip は空文字。
fn daw_clip_display_label(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let ref_json = arg_to_string(args, 0, ctx)?;
    let target: ClipKey = serde_json::from_str(&ref_json)
        .map_err(|e| js_native(format!("clipDisplayLabel: parse: {e}")))?;
    let label = with_app(move |app, _side, _io| {
        let song = &app.cur.song_doc.song();
        let Some(clip) = song.clip_by_key(target) else {
            return String::new();
        };
        crate::widgets::arrangement::view_build::clip_display_label(clip, song).to_string()
    });
    Ok(JsString::from(label.as_str()).into())
}

/// `daw.deviceChain(track_id)` → `host.app.cur.song_doc.song()` の指定トラックの単一デバイス
/// チェーンを、各 device の `{plugin_id, ports}` の JSON 配列文字列で返す。
/// 役割判定はしない (engine は port を順に直結するだけ)。load → migration →
/// port 解決 → 並び順 が production と同じ経路で正しく通ることを JS から
/// end-to-end で検証する。`track_id == MASTER_TRACK_ID` は master_fx_chain を見る。
fn daw_device_chain(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    #[derive(serde::Serialize)]
    struct ChainDevice<'a> {
        plugin_id: &'a str,
        ports: common::port_config::PortConfig,
    }
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let json = with_app(move |app, _side, _io| {
        let devices: &[common::model::Device] =
            if track_id == common::model::MASTER_TRACK_ID {
                &app.cur.song_doc.song().master_fx_chain
            } else {
                app
                    .cur.song_doc.song()
                    .tracks
                    .iter()
                    .find(|t| t.id == track_id)
                    .map(|t| t.devices.as_slice())
                    .unwrap_or(&[])
            };
        // r.md #110: Parallel の中の plugin も信号順で並べる (script は plugin 列だけを見る)。
        let chain: Vec<ChainDevice> = common::model::plugins(devices)
            .map(|d| ChainDevice { plugin_id: d.plugin_id.as_str(), ports: d.ports })
            .collect();
        serde_json::to_string(&chain)
    })
    .map_err(|e| js_native(format!("deviceChain: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

/// `daw.relocateDevices(srcTrack, srcIndicesJson, destTrack, destIndex, copy)`
/// — r.md #71 (プラグインのコピー / 移動) の運搬を script から起こす。
///
/// device のアドレス指定は script の慣習に合わせて **「n 番目の device」**
/// (`resolve_device_id` が安定 id に直す)。 `srcIndices` は `[0, 2]` のような
/// JSON 配列で、チェーン表示順に並べて渡す。
///
/// 呼ぶのは `relocate_devices_inner` (= Song 側処理) で、
/// [`AppData::relocate_devices`] の **plugin state round-trip 待ち**は経由しない。
/// script mode には `AllPluginStates` を pump する口が無く、待たせると永久に
/// 完了しないため。 round-trip 込みの経路は headless test
/// (`daw_gui/tests/app_state/device_relocate.rs`) が応答を fake して押さえている。
/// undo step は round-trip の完了 (`on_all_states_from_child`) と同じく、操作名の付いた
/// 独立した 1 step で積む。
fn daw_relocate_devices(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let src_track = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let indices_json = arg_to_string(args, 1, ctx)?;
    let dest_track = u32::try_from_js(args.get_or_undefined(2), ctx)?;
    let dest_index = u32::try_from_js(args.get_or_undefined(3), ctx)?;
    let copy = bool::try_from_js(args.get_or_undefined(4), ctx).unwrap_or(false);
    let indices: Vec<u32> = serde_json::from_str(&indices_json)
        .map_err(|e| js_native(format!("relocateDevices: parse indices: {e}")))?;
    // JsError は Send でないので String で返し、script スレッドで包む。
    with_app(move |app, _side, _io| -> Result<(), String> {
        let mut device_ids = Vec::with_capacity(indices.len());
        for index in indices {
            let Some(id) = device_id_at_app(app, src_track, index) else {
                return Err(format!("relocateDevices: no device at (track {src_track}, index {index})"));
            };
            device_ids.push(id);
        }
        let req = crate::app::RelocateDevices {
            device_ids,
            dest: common::model::ChainRef::Track(dest_track),
            dest_index: crate::app::InsertAt::Index(dest_index),
            copy,
        };
        let label = crate::event_device::DeviceEvent::RelocateDevices(req.clone()).undo_label();
        let gesture = app.cur.song_doc.enter_own_gesture(label);
        app.relocate_devices_inner(&req);
        app.cur.song_doc.leave_own_gesture(gesture);
        Ok(())
    })
    .map_err(js_native)?;
    Ok(JsValue::undefined())
}

fn daw_set_selection(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let refs: Vec<ClipKey> = serde_json::from_str(&json)
        .map_err(|e| js_native(format!("setSelection: parse: {e}")))?;
    with_app(move |app, _side, _io| {
        // 消えた key は落とす (住所は安定 id なので変換は要らない)。
        let keys: Vec<common::model::ClipKey> =
            refs.iter().filter_map(|r| app.live_clip_key(*r)).collect();
        // 選択の SSoT は時間範囲 1 本 — クリップ選択はその特殊形として張り直す
        // (`docs/plan_range_selection.md`)。 production の click と同じ経路を通す。
        app.handle_event(crate::app::AppEvent::SetClipSelection(keys));
    });
    Ok(JsValue::undefined())
}

/// `daw.setTimeSelection(startBeat, endBeat, trackIdsJson)` — 選択の SSoT である
/// **時間範囲 × レーン**を直接張る (`docs/plan_range_selection.md`)。
///
/// `setSelection` はクリップ選択 (= 範囲の特殊形) しか表せないので、**クリップ境界に
/// 揃っていない範囲**を要求する操作 (範囲 Glue / 範囲削除 / 範囲書き出し) を headless
/// で駆動できなかった。production の drag と同じ `set_time_selection` を通す。
fn daw_set_time_selection(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let start = f64::try_from_js(args.get_or_undefined(0), ctx)?;
    let end = f64::try_from_js(args.get_or_undefined(1), ctx)?;
    let ids_json = arg_to_string(args, 2, ctx)?;
    let ids: Vec<u32> = serde_json::from_str(&ids_json)
        .map_err(|e| js_native(format!("setTimeSelection: parse ids: {e}")))?;
    let lanes: Vec<common::model::LaneRef> =
        ids.into_iter().map(common::model::LaneRef::Track).collect();
    with_app(move |app, _side, _io| {
        app
            .set_time_selection(common::model::TimeSelection::new(start, end, lanes));
    });
    Ok(JsValue::undefined())
}

/// `daw.duplicateTracks(idsJson, linked)` — r.md #30 のトラック複製を production の
/// 右クリックメニュー / D・Alt+D と同じ AppEvent 経路で発火する。`idsJson` は複製対象
/// の track id 配列 (`[1,4]`)、 `linked=true` はリンク複製 (D 相当)、 `false` は独立複製
/// (Alt+D 相当)。selected_track_ids を設定してから dispatch する (GUI の選択状態を模倣)。
fn daw_duplicate_tracks(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ids_json = arg_to_string(args, 0, ctx)?;
    let ids: Vec<u32> = serde_json::from_str(&ids_json)
        .map_err(|e| js_native(format!("duplicateTracks: parse ids: {e}")))?;
    let linked = args.get_or_undefined(1).to_boolean();
    with_app(move |app, _side, _io| {
        app.set_track_selection(ids.clone());
        let ev = if linked {
            AppEvent::DuplicateTracksShared(ids)
        } else {
            AppEvent::DuplicateTracksUnique(ids)
        };
        app.handle_event(ev);
    });
    Ok(JsValue::undefined())
}

fn daw_set_hover_clip(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // 引数は JSON 文字列。 "null" or `{"track_id":N,"clip_id":N}` (安定 id)。
    let json = arg_to_string(args, 0, ctx)?;
    let trimmed = json.trim();
    let cref: Option<ClipKey> = if trimmed == "null" || trimmed.is_empty() {
        None
    } else {
        Some(
            serde_json::from_str(trimmed)
                .map_err(|e| js_native(format!("setHoverClip: parse: {e}")))?,
        )
    };
    with_app(move |app, _side, _io| app.cur.peph.arrangement_hover_clip = cref);
    Ok(JsValue::undefined())
}

fn daw_set_hover_beat(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // 数値 or null。 to_number(ctx)? は null/undefined → NaN なので NaN
    // チェックで None を表現する。
    let arg = args.get_or_undefined(0);
    let beat: Option<f64> = if arg.is_null() || arg.is_undefined() {
        None
    } else {
        let n = arg.to_number(ctx)?;
        if n.is_nan() { None } else { Some(n) }
    };
    with_app(move |app, _side, _io| {
        app.cur.peph.arrangement_hover_beat = beat;
        app.cur.peph.arrangement_hover_beat_raw = beat;
    });
    Ok(JsValue::undefined())
}

fn daw_dispatch_split(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let snap = args.get_or_undefined(0).to_boolean();
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::SplitJoin(crate::event_split::SplitJoinEvent::Split {
            surface: crate::event_split::SplitSurface::Clips,
            at: crate::event_split::SplitAt::Cursor { snap },
        }));
    });
    Ok(JsValue::undefined())
}

fn daw_dispatch_glue(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::SplitJoin(crate::event_split::SplitJoinEvent::Join {
            surface: crate::event_split::SplitSurface::Clips,
        }));
    });
    Ok(JsValue::undefined())
}

// Bounce (with FX): production のクリップ右クリック "Bounce (with FX)" と同じ AppEvent。
// 完了 (新トラックの追加) は非同期なので、呼び手が `inspectSongJson` で待つ。
fn daw_dispatch_bounce_with_fx(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let ref_json = arg_to_string(args, 0, ctx)?;
    let target: ClipKey = serde_json::from_str(&ref_json)
        .map_err(|e| js_native(format!("dispatchBounceWithFx: parse: {e}")))?;
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::BounceClipWithFx(target));
    });
    Ok(JsValue::undefined())
}

// clip rename: production の右クリック "Rename" / F2 と同じ AppEvent 列
// (Begin → Changed → Commit) を 1 回で発火する。 commit ロジック
// (trim / 空文字 no-op) を JS smoke test で verify する用。
fn daw_dispatch_rename_clip(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let ref_json = arg_to_string(args, 0, ctx)?;
    let target: ClipKey = serde_json::from_str(&ref_json)
        .map_err(|e| js_native(format!("dispatchRenameClip: parse: {e}")))?;
    let new_name = arg_to_string(args, 1, ctx)?;
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::BeginRenameClip(target));
        app.handle_event(AppEvent::RenameClipChanged(new_name));
        app.handle_event(AppEvent::CommitRenameClip);
    });
    Ok(JsValue::undefined())
}



