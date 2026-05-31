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

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use boa_engine::property::Attribute;
use boa_engine::value::TryFromJs;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source,
    js_string,
};
use common::model::Song;
use common::plugin_format::PluginFormat;
use common::protocol::{ChildToMain, MainToChild, PluginSlot};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::bootstrap::Bootstrap;
use crate::dispatcher::{NoopJobDispatcher, RecordingDispatcher};

thread_local! {
    /// Running script の host state。 `run_scripted` が `Some(...)` をセットし、
    /// 終了時に `None` に戻す。 native function は `HOST.with_borrow_mut(...)`
    /// で取り出す。
    static HOST: RefCell<Option<ScriptHost>> = const { RefCell::new(None) };
}

/// production binary が `--script <path>` で呼ばれたときの entry。
/// `output_override` は `--output <path>` を script から `daw.scriptArgs.output`
/// として参照可能にするため。 runtime 終了で exit code 0 / JS error で 1。
pub fn run_scripted(
    bootstrap: Bootstrap,
    script_path: &Path,
    output_override: Option<&Path>,
    extra_args: &[(String, String)],
) -> Result<()> {
    let source = std::fs::read_to_string(script_path)
        .with_context(|| format!("failed to read script {}", script_path.display()))?;
    HOST.with_borrow_mut(|h| {
        *h = Some(ScriptHost::new(
            bootstrap,
            output_override.map(PathBuf::from),
            extra_args.to_vec(),
        ));
    });

    let result = (|| -> Result<()> {
        let mut ctx = Context::default();
        register_daw_globals(&mut ctx)?;
        let parsed = Source::from_bytes(source.as_bytes());
        ctx.eval(parsed)
            .map_err(|e| anyhow!("script error: {e}"))?;
        Ok(())
    })();

    HOST.with_borrow_mut(|h| {
        *h = None;
    });
    result
}

/// boa context が握る host state。 IPC channel は Bootstrap 内に。
struct ScriptHost {
    bootstrap: Bootstrap,
    /// `--output` etc. を script に渡すための args bag。
    script_args: ScriptArgs,
    /// 直前の `loadSongFromObject` で送った Song を keep。 `setTrackLatency`
    /// など差分更新が必要な API のために。
    last_loaded_song: Option<Song>,
    /// PR7 follow-up (JS test infra): GUI mode の `AppData` と同じ役割を
    /// script mode でも持つ。 AppEvent を script から発火できるように
    /// するため、 production の `AppData::handle_event` を直接呼ぶ
    /// (= `daw.dispatchSplit` / `daw.glueSelectedClips` 等の API は
    /// app.handle_event 経由)。 dispatcher は test 用の Recording / Noop
    /// を使う (winit event loop 無し)。
    app: AppData,
    /// PR3.3: GUI mode の `AppData` と同じ役割。 `pump_until` 内で
    /// `SlotPluginLoaded` を見たときに `(plugin_id → track_id)` を覚えて
    /// おき、 `PluginLatencyChanged` 受信時に track の累積 latency を
    /// 計算して `last_loaded_song` を更新 → `LoadSong` を再送する。
    plugin_to_track: std::collections::HashMap<u32, u32>,
    plugin_latencies: std::collections::HashMap<u32, u32>,
    track_plugin_ids: std::collections::HashMap<u32, Vec<u32>>,
}

#[derive(Default, Clone)]
struct ScriptArgs {
    output: Option<PathBuf>,
    /// Free-form `--arg KEY=VALUE` pairs from the CLI, exposed as
    /// `daw.scriptArgs[key]` properties.
    extra: Vec<(String, String)>,
}

impl ScriptHost {
    fn new(
        bootstrap: Bootstrap,
        output: Option<PathBuf>,
        extra: Vec<(String, String)>,
    ) -> Self {
        // AppData::new は audio_tx / plugin_tx の clone を要求する。
        // bootstrap 内の sender は production と同形なのでそのまま渡せる
        // (= app.handle_event 内の send_audio / send_plugin がそのまま
        // bootstrap が握る IPC channel に流れる)。 dispatcher は test
        // 用 noop / recording。 `_proxy` を返す Recording 実装を
        // BackgroundDispatcher として渡し、 background thread は
        // script では使わないので spawn されない (AppData の API を
        // 同期呼び出しするだけ)。
        let app = AppData::new(
            bootstrap.audio_tx.clone(),
            bootstrap.plugin_tx.clone(),
            None,
            None,
            RecordingDispatcher::new(),
            Arc::new(NoopJobDispatcher),
            // script mode は同 process 内の bootstrap が握る supervisor を
            // 渡しても安全だが、 script 中に子プロセスが死ぬケースは
            // テスト・録画用途では発生しない前提なので None で十分。
            None,
            // production と同じ実データディレクトリ (= 既存挙動を維持)。
            common::app_dirs::AppDirs::production(),
        );
        Self {
            bootstrap,
            script_args: ScriptArgs { output, extra },
            last_loaded_song: None,
            plugin_to_track: std::collections::HashMap::new(),
            plugin_latencies: std::collections::HashMap::new(),
            track_plugin_ids: std::collections::HashMap::new(),
            app,
        }
    }

    /// `incoming_rx` から条件 `pred` を満たす event が来るまで pump。
    /// 他の event は副作用処理して drain (production GUI mode の
    /// `spawn_incoming_bridge` 相当):
    ///   - `SlotPluginLoaded` → `OpenPluginShmem` を audio に forward + plugin
    ///     ↔ track を local map に記録
    ///   - `PluginLatencyChanged` → plugin latency を local map に積み、
    ///     track 累積を recompute、 `last_loaded_song` を更新して
    ///     `LoadSong` を audio に再送 (PR3.3 PDC 反映経路)
    ///   - `SlotPluginUnloaded` → plugin_id を 3 つの local map から退避
    ///
    /// timeout を超えたら `Err`。
    fn pump_until<F>(&mut self, mut pred: F, timeout: Duration) -> Result<ChildToMain>
    where
        F: FnMut(&ChildToMain) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            // tokio mpsc の try_recv は &mut self を要求。 split-borrow で
            // `audio_tx` と `incoming_rx` を別々に持つため、 receive ごとに
            // local 変数に取り出す。
            let recv_result = self
                .bootstrap
                .incoming_rx
                .as_mut()
                .expect("Bootstrap.incoming_rx already taken (GUI mode)")
                .try_recv();
            match recv_result {
                Ok(msg) => {
                    self.handle_incoming(&msg);
                    if pred(&msg) {
                        return Ok(msg);
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("pump_until timed out"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(anyhow!("incoming pipe closed"));
                }
            }
        }
    }

    fn handle_incoming(&mut self, msg: &ChildToMain) {
        match msg {
            ChildToMain::SlotPluginLoaded {
                track,
                plugin_id,
                shmem_id,
                slot,
                ..
            } => {
                let _ = self.bootstrap.audio_tx.send(MainToChild::OpenPluginShmem {
                    plugin_id: *plugin_id,
                    shmem_id: shmem_id.clone(),
                    track: *track,
                    slot: *slot,
                });
                self.plugin_to_track.insert(*plugin_id, *track);
                self.track_plugin_ids
                    .entry(*track)
                    .or_default()
                    .push(*plugin_id);
            }
            ChildToMain::SlotPluginUnloaded { plugin_id } => {
                self.plugin_latencies.remove(plugin_id);
                self.plugin_to_track.remove(plugin_id);
                for v in self.track_plugin_ids.values_mut() {
                    v.retain(|p| p != plugin_id);
                }
                self.track_plugin_ids.retain(|_, v| !v.is_empty());
                self.recompute_track_latencies();
            }
            ChildToMain::PluginLatencyChanged { plugin_id, samples } => {
                self.plugin_latencies.insert(*plugin_id, *samples);
                self.recompute_track_latencies();
            }
            ChildToMain::SlotPluginLoadFailed {
                track,
                slot,
                plugin_id,
                reason,
            } => {
                tracing::error!(
                    track,
                    ?slot,
                    %plugin_id,
                    %reason,
                    "script: plugin load failed"
                );
            }
            _ => {}
        }
    }

    /// pending な incoming events を `for_duration` だけ drain する。
    /// 主に `exportWav` の前に PluginLatencyChanged を取り込んで song を
    /// 最新化するために使う (export thread が song を snapshot する前に
    /// LoadSong を届けたい)。 期間内に新規 event が無くても block しない。
    fn drain_pending_for(&mut self, for_duration: Duration) {
        let deadline = Instant::now() + for_duration;
        while Instant::now() < deadline {
            let recv = self
                .bootstrap
                .incoming_rx
                .as_mut()
                .expect("Bootstrap.incoming_rx already taken (GUI mode)")
                .try_recv();
            match recv {
                Ok(msg) => self.handle_incoming(&msg),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// AppData::recompute_track_latencies の script-mode mirror。 song の
    /// 各 track について plugin latencies を sum し、 値が変わったら
    /// `LoadSong` を audio に再送して PDC を反映させる。
    fn recompute_track_latencies(&mut self) {
        let Some(song) = self.last_loaded_song.as_mut() else {
            return;
        };
        let mut changed = false;
        for (track_id, plugin_ids) in &self.track_plugin_ids {
            let total: u32 = plugin_ids
                .iter()
                .map(|pid| self.plugin_latencies.get(pid).copied().unwrap_or(0))
                .sum();
            if let Some(t) = song.tracks.iter_mut().find(|t| t.id == *track_id)
                && t.reported_latency_samples != total
            {
                t.reported_latency_samples = total;
                changed = true;
            }
        }
        let track_ids_with_plugins: std::collections::HashSet<u32> =
            self.track_plugin_ids.keys().copied().collect();
        for t in &mut song.tracks {
            if !track_ids_with_plugins.contains(&t.id)
                && t.reported_latency_samples != 0
            {
                t.reported_latency_samples = 0;
                changed = true;
            }
        }
        if changed {
            let cloned = song.clone();
            let _ = self.bootstrap.audio_tx.send(MainToChild::LoadSong(cloned.clone()));
            let _ = self.bootstrap.plugin_tx.send(MainToChild::LoadSong(cloned));
        }
    }
}

/// `HOST` を borrow_mut してクロージャを実行する短縮ヘルパ。
fn with_host<F, R>(f: F) -> R
where
    F: FnOnce(&mut ScriptHost) -> R,
{
    HOST.with_borrow_mut(|h| {
        let host = h.as_mut().expect("script host not initialized");
        f(host)
    })
}

// ---------------------------------------------------------------------------
// `daw.*` global の登録
// ---------------------------------------------------------------------------

fn register_daw_globals(ctx: &mut Context) -> Result<()> {
    let daw = boa_engine::object::ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(daw_load_song_from_object),
            js_string!("loadSongFromObject"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_slot_plugin),
            js_string!("setSlotPlugin"),
            6,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_wait_for_plugin_loaded),
            js_string!("waitForPluginLoaded"),
            4,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_generated_audio),
            js_string!("setGeneratedAudio"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_export_wav),
            js_string!("exportWav"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_track_latency),
            js_string!("setTrackLatency"),
            2,
        )
        // ----- PR7 follow-up (JS test infra) ----------------------------
        // app.* の API は ScriptHost::app (= AppData) を直接 mutate して
        // production と同じ AppEvent handler を回す。 IPC は AppData の
        // 内部 send_audio / send_plugin から bootstrap の channel に
        // 流れる。 production GUI mode と挙動を一致させる。
        .function(
            NativeFunction::from_fn_ptr(daw_app_load_song_json),
            js_string!("appLoadSongJson"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_inspect_song_json),
            js_string!("inspectSongJson"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_selection),
            js_string!("setSelection"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_hover_clip),
            js_string!("setHoverClip"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_hover_beat),
            js_string!("setHoverBeat"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_dispatch_split),
            js_string!("dispatchSplit"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_dispatch_glue),
            js_string!("dispatchGlue"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_dispatch_rename_clip),
            js_string!("dispatchRenameClip"),
            2,
        )
        // ----- Phase 7 B5 Scale & Root API ------------------------------
        .function(
            NativeFunction::from_fn_ptr(daw_set_scale_at_playhead),
            js_string!("setScaleAtPlayhead"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_clear_scale_changes),
            js_string!("clearScaleChanges"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_toggle_snap_on_draw),
            js_string!("toggleSnapOnDraw"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_toggle_snap_live_input),
            js_string!("toggleSnapLiveInput"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_toggle_fold_to_scale),
            js_string!("toggleFoldToScale"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_quantize_pitches_to_scale),
            js_string!("quantizePitchesToScale"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_add_note),
            js_string!("addNote"),
            5,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_set_note_positions_json),
            js_string!("setNotePositionsJson"),
            1,
        )
        .build();

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

    let song: Song = serde_json::from_str(&json_str).map_err(|e| {
        JsError::from_native(JsNativeError::error().with_message(format!("song JSON parse: {e}")))
    })?;
    with_host(|h| {
        let _ = h.bootstrap.audio_tx.send(MainToChild::LoadSong(song.clone()));
        let _ = h.bootstrap.plugin_tx.send(MainToChild::LoadSong(song.clone()));
        h.last_loaded_song = Some(song);
    });
    Ok(JsValue::undefined())
}

fn daw_set_slot_plugin(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let slot_kind = u8::try_from_js(args.get_or_undefined(1), ctx)?;
    let slot_index = u32::try_from_js(args.get_or_undefined(2), ctx)?;
    let format_str = String::try_from_js(args.get_or_undefined(3), ctx)?;
    let path_str = String::try_from_js(args.get_or_undefined(4), ctx)?;
    let plugin_id = String::try_from_js(args.get_or_undefined(5), ctx)?;

    let slot = match slot_kind {
        0 => PluginSlot::MidiFx(slot_index),
        1 => PluginSlot::Instrument,
        2 => PluginSlot::Fx(slot_index),
        n => {
            return Err(JsNativeError::range()
                .with_message(format!("invalid slot_kind {n} (0=MidiFx,1=Instrument,2=Fx)"))
                .into());
        }
    };
    let format = match format_str.as_str() {
        "clap" => PluginFormat::Clap,
        "vst3" => PluginFormat::Vst3,
        s => {
            return Err(JsNativeError::range()
                .with_message(format!("invalid format {s:?}; expected 'clap' or 'vst3'"))
                .into());
        }
    };
    with_host(|h| {
        let _ = h.bootstrap.plugin_tx.send(MainToChild::SetSlotPlugin {
            track: track_id,
            slot,
            format,
            path: PathBuf::from(path_str),
            plugin_id,
            initial_state: None,
        });
    });
    Ok(JsValue::undefined())
}

fn daw_wait_for_plugin_loaded(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let slot_kind = u8::try_from_js(args.get_or_undefined(1), ctx)?;
    let slot_index = u32::try_from_js(args.get_or_undefined(2), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(3), ctx).unwrap_or(30_000);
    let want_slot = match slot_kind {
        0 => PluginSlot::MidiFx(slot_index),
        1 => PluginSlot::Instrument,
        2 => PluginSlot::Fx(slot_index),
        n => {
            return Err(JsNativeError::range()
                .with_message(format!("invalid slot_kind {n}"))
                .into());
        }
    };

    let res = with_host(|h| {
        h.pump_until(
            |msg| {
                matches!(
                    msg,
                    ChildToMain::SlotPluginLoaded { track, slot, .. }
                        if *track == track_id && *slot == want_slot
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
/// `MainToChild::SetGeneratedAudio` 経路を IPC から削除したため。
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

    let pump_result = with_host(|h| {
        // PR3.3: 直前に発火された IPC events (PluginLatencyChanged 等) を
        // exportWav の前に drain して、 latency が `last_loaded_song` →
        // `LoadSong` 再送経路で `compile_schedule` まで反映されるのを待つ。
        // export thread は ExportWav arrival 時点で `shared.song` を snapshot
        // するので、 event drain して LoadSong を先に届けないと PDC が
        // 適用されない song で render が始まる。
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.bootstrap.audio_tx.send(MainToChild::ExportWav {
            path: PathBuf::from(path_str),
        });
        h.pump_until(
            |msg| matches!(msg, ChildToMain::ExportWavComplete { .. }),
            Duration::from_millis(timeout_ms),
        )
    });
    match pump_result {
        Ok(ChildToMain::ExportWavComplete { error: None }) => Ok(JsValue::undefined()),
        Ok(ChildToMain::ExportWavComplete { error: Some(e) }) => Err(JsError::from_native(
            JsNativeError::error().with_message(format!("export failed: {e}")),
        )),
        Ok(_) => unreachable!(),
        Err(e) => Err(JsError::from_native(
            JsNativeError::error().with_message(format!("exportWav: {e}")),
        )),
    }
}

fn daw_set_track_latency(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let track_id = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let samples = u32::try_from_js(args.get_or_undefined(1), ctx)?;

    let res = with_host(|h| -> Result<()> {
        let song = h
            .last_loaded_song
            .as_mut()
            .ok_or_else(|| anyhow!(
                "setTrackLatency requires loadSongFromObject to have been called first"
            ))?;
        if let Some(t) = song.track_by_id_mut(track_id) {
            t.reported_latency_samples = samples;
        }
        let cloned = song.clone();
        let _ = h.bootstrap.audio_tx.send(MainToChild::LoadSong(cloned.clone()));
        let _ = h.bootstrap.plugin_tx.send(MainToChild::LoadSong(cloned));
        Ok(())
    });
    res.map_err(|e| {
        JsError::from_native(JsNativeError::error().with_message(format!("setTrackLatency: {e}")))
    })?;
    Ok(JsValue::undefined())
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
    let mut song: Song = serde_json::from_str(&json)
        .map_err(|e| js_native(format!("appLoadSongJson: parse: {e}")))?;
    song.ensure_ids();
    song.ensure_clip_contents();
    song.ensure_audio_source_ids();
    with_host(|host| {
        host.app.song = song;
        host.app.sync_song_to_plugin_host();
    });
    Ok(JsValue::undefined())
}

fn daw_inspect_song_json(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = with_host(|host| serde_json::to_string(&host.app.song))
        .map_err(|e| js_native(format!("inspectSongJson: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

fn daw_set_selection(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let refs: Vec<ClipRef> = serde_json::from_str(&json)
        .map_err(|e| js_native(format!("setSelection: parse: {e}")))?;
    with_host(|host| {
        host.app.selected_clip = refs.last().copied();
        host.app.selected_clips = refs;
    });
    Ok(JsValue::undefined())
}

fn daw_set_hover_clip(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // 引数は JSON 文字列。 "null" or `{"track":N,"clip":N}`。
    let json = arg_to_string(args, 0, ctx)?;
    let trimmed = json.trim();
    let cref: Option<ClipRef> = if trimmed == "null" || trimmed.is_empty() {
        None
    } else {
        Some(
            serde_json::from_str(trimmed)
                .map_err(|e| js_native(format!("setHoverClip: parse: {e}")))?,
        )
    };
    with_host(|host| host.app.arrangement_hover_clip = cref);
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
    with_host(|host| {
        host.app.arrangement_hover_beat = beat;
        host.app.arrangement_hover_beat_raw = beat;
    });
    Ok(JsValue::undefined())
}

fn daw_dispatch_split(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let snap = args.get_or_undefined(0).to_boolean();
    with_host(|host| {
        host.app
            .handle_event(AppEvent::SplitClipAtPlayhead { snap });
    });
    Ok(JsValue::undefined())
}

fn daw_dispatch_glue(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::GlueSelectedClips);
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
    let target: ClipRef = serde_json::from_str(&ref_json)
        .map_err(|e| js_native(format!("dispatchRenameClip: parse: {e}")))?;
    let new_name = arg_to_string(args, 1, ctx)?;
    with_host(|host| {
        host.app.handle_event(AppEvent::BeginRenameClip(target));
        host.app.handle_event(AppEvent::RenameClipChanged(new_name));
        host.app.handle_event(AppEvent::CommitRenameClip);
    });
    Ok(JsValue::undefined())
}

// ============================================================================
// Phase 7 B5 (`docs/plan_scale.html`): Scale & Root の JS smoke test API
// ============================================================================
//
// production GUI mode の Transport bar / piano_roll toggle と同じ AppEvent を
// 発火する。 JS smoke test (`tests/scripts/scale_smoke.js`) で
// scale_changes の編集 / snap apply / quantize / fold mode の挙動を verify。

fn scale_from_name(name: &str) -> Option<common::scale::Scale> {
    use common::scale::Scale;
    match name {
        "Major" => Some(Scale::Major),
        "NaturalMinor" | "Minor" => Some(Scale::NaturalMinor),
        "Dorian" => Some(Scale::Dorian),
        "Phrygian" => Some(Scale::Phrygian),
        "Lydian" => Some(Scale::Lydian),
        "Mixolydian" => Some(Scale::Mixolydian),
        "Locrian" => Some(Scale::Locrian),
        "HarmonicMinor" => Some(Scale::HarmonicMinor),
        "MelodicMinor" => Some(Scale::MelodicMinor),
        "MajorPentatonic" => Some(Scale::MajorPentatonic),
        "MinorPentatonic" => Some(Scale::MinorPentatonic),
        "Blues" => Some(Scale::Blues),
        "WholeTone" => Some(Scale::WholeTone),
        "Diminished" => Some(Scale::Diminished),
        "HalfWholeDim" => Some(Scale::HalfWholeDim),
        "Chromatic" => Some(Scale::Chromatic),
        "HarmonicMajor" => Some(Scale::HarmonicMajor),
        "DoubleHarmonic" => Some(Scale::DoubleHarmonic),
        "LydianDominant" => Some(Scale::LydianDominant),
        "PhrygianDominant" => Some(Scale::PhrygianDominant),
        "HungarianMinor" => Some(Scale::HungarianMinor),
        "Japanese" => Some(Scale::Japanese),
        _ => None,
    }
}

fn daw_set_scale_at_playhead(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let root = args.get_or_undefined(0).to_number(ctx)? as u8;
    let scale_name = args
        .get_or_undefined(1)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("scale name not utf8: {e}")))?;
    let scale = scale_from_name(&scale_name).ok_or_else(|| {
        JsNativeError::typ().with_message(format!("unknown scale name: {scale_name}"))
    })?;
    with_host(|host| {
        host.app
            .handle_event(AppEvent::SetScaleAtPlayhead { root, scale });
    });
    Ok(JsValue::undefined())
}

fn daw_clear_scale_changes(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ClearScaleChanges);
    });
    Ok(JsValue::undefined())
}

fn daw_toggle_snap_on_draw(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleSnapOnDraw);
    });
    Ok(JsValue::undefined())
}

fn daw_toggle_snap_live_input(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleSnapLiveInput);
    });
    Ok(JsValue::undefined())
}

fn daw_toggle_fold_to_scale(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    with_host(|host| {
        host.app.handle_event(AppEvent::ToggleFoldToScale);
    });
    Ok(JsValue::undefined())
}

fn daw_quantize_pitches_to_scale(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    use crate::app::QuantizePitchTarget;
    let target_name = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("target name not utf8: {e}")))?;
    let target = match target_name.as_str() {
        "selected_notes" => QuantizePitchTarget::SelectedNotes,
        "selected_clip_all_notes" => QuantizePitchTarget::SelectedClipAllNotes,
        other => {
            return Err(JsNativeError::typ()
                .with_message(format!("unknown quantize target: {other}"))
                .into());
        }
    };
    with_host(|host| {
        host.app
            .handle_event(AppEvent::QuantizePitchesToScale(target));
    });
    Ok(JsValue::undefined())
}

fn daw_add_note(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let track = args.get_or_undefined(0).to_number(ctx)? as u32;
    let clip = args.get_or_undefined(1).to_number(ctx)? as u32;
    let start_beat = args.get_or_undefined(2).to_number(ctx)?;
    let duration = args.get_or_undefined(3).to_number(ctx)?;
    let pitch = args.get_or_undefined(4).to_number(ctx)? as u8;
    with_host(|host| {
        host.app.handle_event(AppEvent::AddNote {
            track,
            clip,
            start_beat,
            duration,
            pitch,
        });
    });
    Ok(JsValue::undefined())
}

fn daw_set_note_positions_json(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let json = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string()
        .map_err(|e| JsNativeError::typ().with_message(format!("entries JSON not utf8: {e}")))?;
    let entries: Vec<(u32, f64, u8)> = serde_json::from_str(&json).map_err(|e| {
        JsNativeError::typ().with_message(format!("entries JSON decode: {e}"))
    })?;
    with_host(|host| {
        host.app.handle_event(AppEvent::SetNotePositions(entries));
    });
    Ok(JsValue::undefined())
}
