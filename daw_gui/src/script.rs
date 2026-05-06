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
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use boa_engine::object::builtins::JsTypedArray;
use boa_engine::property::Attribute;
use boa_engine::value::TryFromJs;
use boa_engine::{
    Context, JsArgs, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source,
    js_string,
};
use common::model::Song;
use common::plugin_format::PluginFormat;
use common::protocol::{ChildToMain, MainToChild, PluginSlot};

use crate::bootstrap::Bootstrap;

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
) -> Result<()> {
    let source = std::fs::read_to_string(script_path)
        .with_context(|| format!("failed to read script {}", script_path.display()))?;
    HOST.with_borrow_mut(|h| {
        *h = Some(ScriptHost::new(bootstrap, output_override.map(PathBuf::from)));
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
}

#[derive(Default, Clone)]
struct ScriptArgs {
    output: Option<PathBuf>,
}

impl ScriptHost {
    fn new(bootstrap: Bootstrap, output: Option<PathBuf>) -> Self {
        Self {
            bootstrap,
            script_args: ScriptArgs { output },
            last_loaded_song: None,
        }
    }

    /// `incoming_rx` から条件 `pred` を満たす event が来るまで pump。
    /// 他の event は `SlotPluginLoaded` のみ副作用処理し (audio に
    /// `OpenPluginShmem` を forward — GUI mode の `spawn_incoming_bridge`
    /// と同等)、 残りは drain して捨てる。 timeout を超えたら `Err`。
    fn pump_until<F>(&mut self, mut pred: F, timeout: Duration) -> Result<ChildToMain>
    where
        F: FnMut(&ChildToMain) -> bool,
    {
        let deadline = Instant::now() + timeout;
        // split borrow: `&mut incoming_rx` と `&audio_tx` を同時に持つため。
        let audio_tx = self.bootstrap.audio_tx.clone();
        let rx = self
            .bootstrap
            .incoming_rx
            .as_mut()
            .expect("Bootstrap.incoming_rx already taken (GUI mode)");
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if let ChildToMain::SlotPluginLoaded {
                        track,
                        slot,
                        plugin_id,
                        shmem_id,
                        ..
                    } = &msg
                    {
                        let _ = audio_tx.send(MainToChild::OpenPluginShmem {
                            plugin_id: *plugin_id,
                            shmem_id: shmem_id.clone(),
                            track: *track,
                            slot: *slot,
                        });
                    }
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
            NativeFunction::from_fn_ptr(daw_set_vocal_audio),
            js_string!("setVocalAudio"),
            5,
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
        .build();

    // `daw.scriptArgs` (= { output: <CLI で指定された --output 値 or null> })
    let args_obj = boa_engine::object::ObjectInitializer::new(ctx).build();
    let output_value: JsValue = HOST.with_borrow(|h| {
        match h.as_ref().and_then(|h| h.script_args.output.as_ref()) {
            Some(p) => JsString::from(p.to_string_lossy().as_ref()).into(),
            None => JsValue::null(),
        }
    });
    args_obj
        .set(js_string!("output"), output_value, false, ctx)
        .map_err(|e| anyhow!("set scriptArgs.output: {e}"))?;
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

fn daw_set_vocal_audio(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let track = u32::try_from_js(args.get_or_undefined(0), ctx)?;
    let clip = u32::try_from_js(args.get_or_undefined(1), ctx)?;
    let clip_start_samples = u64::try_from_js(args.get_or_undefined(2), ctx)?;
    let samples_value = args.get_or_undefined(3).clone();
    let sample_rate = u32::try_from_js(args.get_or_undefined(4), ctx)?;

    let typed = JsTypedArray::from_object(samples_value.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message("samples must be a Float32Array")
    })?)
    .map_err(|_| JsNativeError::typ().with_message("samples must be a Float32Array"))?;
    let len = typed.length(ctx)?;
    let mut samples = Vec::with_capacity(len);
    for i in 0..len {
        let v = typed.at(i as i64, ctx)?;
        samples.push(v.to_number(ctx)? as f32);
    }

    with_host(|h| {
        let _ = h.bootstrap.audio_tx.send(MainToChild::SetVocalAudio {
            track,
            clip,
            clip_start_samples,
            sample_rate,
            samples,
        });
    });
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
