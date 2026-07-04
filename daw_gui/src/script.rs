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
use common::protocol::{AudioCommand, AudioEvent, PluginCommand, PluginEvent};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::bootstrap::{Bootstrap, ChildEvent};
use crate::dispatcher::RecordingDispatcher;

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
    /// `SlotPluginLoaded` を見たときに `(device_id → track_id)` を覚えて
    /// おき、 `PluginLatencyChanged` 受信時に track の累積 latency を
    /// 計算して `last_loaded_song` を更新 → `LoadSong` を再送する
    /// (v29: key は安定 device id)。
    plugin_to_track: std::collections::HashMap<u64, u32>,
    plugin_latencies: std::collections::HashMap<u64, u32>,
    track_plugin_ids: std::collections::HashMap<u32, Vec<u64>>,
    /// v29: 生 `daw.setSlotPlugin` 用の要求 generation counter (AppData の
    /// counter と衝突しないよう ScriptHost 側でも単調増加を維持し、 送信前に
    /// `app.ipc.pending_plugin_loads` へ登録して echo を通す)。
    next_raw_load_generation: u64,
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
            // (review) script mode でも VOICEVOX engine の lazy spawn は起きる
            // (loadSongFile → 歌唱合成)。 Noop だと Job Object 未登録で script
            // プロセス終了後に engine が zombie 化するので production と同じ
            // dispatcher を渡す。
            Arc::new(crate::dispatcher::Win32JobDispatcher::new(Arc::clone(
                &bootstrap.job,
            ))),
            // script mode は同 process 内の bootstrap が握る supervisor を
            // 渡しても安全だが、 script 中に子プロセスが死ぬケースは
            // テスト・録画用途では発生しない前提なので None で十分。
            None,
            // production と同じ実データディレクトリ (= 既存挙動を維持)。
            common::app_dirs::AppDirs::production(),
            // (A1 r.md #8) bootstrap が解決したデバイス実レート。
            bootstrap.sample_rate,
        );
        Self {
            bootstrap,
            script_args: ScriptArgs { output, extra },
            last_loaded_song: None,
            plugin_to_track: std::collections::HashMap::new(),
            plugin_latencies: std::collections::HashMap::new(),
            track_plugin_ids: std::collections::HashMap::new(),
            next_raw_load_generation: 0,
            app,
        }
    }

    /// v29: `(track_id, device_index)` → 安定 device id。 app の song
    /// (loadSongFile 経路) → `last_loaded_song` (loadSongFromObject 経路) の
    /// 順で解決する。
    fn resolve_device_id(&self, track_id: u32, index: u32) -> Option<u64> {
        crate::app::device_id_at(self.app.song_doc.song(), track_id, index).or_else(|| {
            self.last_loaded_song
                .as_ref()
                .and_then(|s| crate::app::device_id_at(s, track_id, index))
        })
    }

    /// 逆方向: device id → `(track_id, device_index)`。
    fn resolve_device_coords(&self, device_id: u64) -> Option<(u32, u32)> {
        crate::app::find_device_by_id(self.app.song_doc.song(), device_id).or_else(|| {
            self.last_loaded_song
                .as_ref()
                .and_then(|s| crate::app::find_device_by_id(s, device_id))
        })
    }

    /// `incoming_rx` から条件 `pred` を満たす event が来るまで pump。
    /// 他の event は副作用処理して drain (production GUI mode の
    /// `spawn_incoming_bridge` 相当):
    ///   - `SlotPluginLoaded` → `OpenPluginShmem` を audio に forward + device
    ///     ↔ track を local map に記録
    ///   - `PluginLatencyChanged` → plugin latency を local map に積み、
    ///     track 累積を recompute、 `last_loaded_song` を更新して
    ///     `LoadSong` を audio に再送 (PR3.3 PDC 反映経路)
    ///   - `SlotPluginUnloaded` → device_id を 3 つの local map から退避
    ///
    /// timeout を超えたら `Err`。
    fn pump_until<F>(&mut self, mut pred: F, timeout: Duration) -> Result<ChildEvent>
    where
        F: FnMut(&ChildEvent) -> bool,
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

    fn handle_incoming(&mut self, msg: &ChildEvent) {
        let ChildEvent::Plugin(msg) = msg else {
            return; // audio 側 event は script bookkeeping に不要。
        };
        match msg {
            PluginEvent::SlotPluginLoaded {
                device_id,
                id,
                name,
                shmem_id,
                state_load_error,
                aux_output_count,
                generation,
            } => {
                // script 専用 bookkeeping (PDC recompute が参照)。
                if let Some((track, _index)) = self.resolve_device_coords(*device_id) {
                    self.plugin_to_track.insert(*device_id, track);
                    self.track_plugin_ids
                        .entry(track)
                        .or_default()
                        .push(*device_id);
                }
                // GUI runner と同じく app へ dispatch する。これで `loaded_slots` が
                // 埋まり、OpenPluginShmem 送信 + `sync_vocal_metadata` 再 flush
                // (= builtin VOICEVOX の歌唱/読み上げ合成 trigger) が走る。これが
                // 無いと、 slot ロード前の初回 flush が skip されたまま再 flush されず、
                // VOICEVOX を含む project の headless export が無音になる
                // (`docs/plan_voicevox_talk.md` §7 で talk export 検証時に発覚)。
                self.app
                    .handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoaded {
                        device_id: *device_id,
                        id: id.clone(),
                        name: name.clone(),
                        shmem_id: shmem_id.clone(),
                        state_load_error: state_load_error.clone(),
                        aux_output_count: *aux_output_count,
                        generation: *generation,
                    }));
            }
            PluginEvent::SlotPluginUnloaded { device_id } => {
                self.plugin_latencies.remove(device_id);
                self.plugin_to_track.remove(device_id);
                for v in self.track_plugin_ids.values_mut() {
                    v.retain(|p| p != device_id);
                }
                self.track_plugin_ids.retain(|_, v| !v.is_empty());
                self.recompute_track_latencies();
            }
            PluginEvent::PluginLatencyChanged { device_id, samples } => {
                self.plugin_latencies.insert(*device_id, *samples);
                self.recompute_track_latencies();
            }
            PluginEvent::SlotPluginLoadFailed {
                device_id,
                plugin_id,
                reason,
                generation: _,
            } => {
                tracing::error!(
                    device_id,
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
            // v29: Song を受けるのは daw_audio のみ (plugin_host は order-free
            // な flat map になり LoadSong variant 自体が無い)。
            let cloned = song.clone();
            let _ = self.bootstrap.audio_tx.send(AudioCommand::LoadSong(cloned));
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
        // ----- headless export-range test harness ----------------
        .function(
            NativeFunction::from_fn_ptr(daw_load_song_file),
            js_string!("loadSongFile"),
            1,
        )
        .function(NativeFunction::from_fn_ptr(daw_play), js_string!("play"), 0)
        .function(NativeFunction::from_fn_ptr(daw_stop), js_string!("stop"), 0)
        .function(
            NativeFunction::from_fn_ptr(daw_sleep_ms),
            js_string!("sleepMs"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_reinit_for_export),
            js_string!("reinitForExport"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_export_wav_range),
            js_string!("exportWavRange"),
            4,
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
            NativeFunction::from_fn_ptr(daw_clip_display_label),
            js_string!("clipDisplayLabel"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(daw_device_chain),
            js_string!("deviceChain"),
            1,
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

    let mut song: Song = serde_json::from_str(&json_str).map_err(|e| {
        JsError::from_native(JsNativeError::error().with_message(format!("song JSON parse: {e}")))
    })?;
    // v29: 安定 id (track / device / send / note...) を採番してから流す。
    // device_id addressing (`daw.setSlotPlugin` 等) がこの id を引く。
    song.ensure_ids();
    with_host(|h| {
        let _ = h.bootstrap.audio_tx.send(AudioCommand::LoadSong(song.clone()));
        h.last_loaded_song = Some(song);
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
    with_host(|h| -> JsResult<()> {
        // v29: 安定 device id でアドレスする。 事前に loadSongFromObject /
        // loadSongFile 済みの song から解決する (ensure_ids 済み)。
        let Some(device_id) = h.resolve_device_id(track_id, index) else {
            return Err(js_native(format!(
                "setSlotPlugin: no device at (track {track_id}, index {index}) — \
                 load a song with that device first (v29 requires stable device ids)"
            )));
        };
        // generation を採番して app の pending にも登録 (= 応答の echo が
        // AppData の世代 guard を通り、 OpenPluginShmem forward まで走る)。
        h.next_raw_load_generation = h.next_raw_load_generation.wrapping_add(1).max(1);
        let generation = h.next_raw_load_generation;
        h.app.ipc.pending_plugin_loads.insert(device_id, generation);
        let _ = h.bootstrap.plugin_tx.send(PluginCommand::SetSlotPlugin {
            device_id,
            track_id,
            format,
            path: PathBuf::from(path_str),
            plugin_id,
            initial_state: None,
            generation,
        });
        Ok(())
    })?;
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

    let res = with_host(|h| -> Result<ChildEvent> {
        // v29: 応答は device_id keyed。 期待 device を先に解決して照合する。
        let want_device = h
            .resolve_device_id(track_id, want_index)
            .ok_or_else(|| anyhow!(
                "waitForPluginLoaded: no device at (track {track_id}, index {want_index})"
            ))?;
        h.pump_until(
            |msg| {
                matches!(
                    msg,
                    ChildEvent::Plugin(PluginEvent::SlotPluginLoaded { device_id, .. })
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

    let pump_result = with_host(|h| {
        // PR3.3: 直前に発火された IPC events (PluginLatencyChanged 等) を
        // exportWav の前に drain して、 latency が `last_loaded_song` →
        // `LoadSong` 再送経路で `compile_schedule` まで反映されるのを待つ。
        // export thread は ExportWav arrival 時点で `shared.song` を snapshot
        // するので、 event drain して LoadSong を先に届けないと PDC が
        // 適用されない song で render が始まる。
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.bootstrap.audio_tx.send(AudioCommand::ExportWav {
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
    with_host(|h| {
        // Resolve plugin ids → DLL paths from the cached DB the bootstrap built.
        h.app.ipc.plugin_db = h.bootstrap.plugin_db.clone();
        h.app.song_doc.file_path = Some(path.clone());
        // Instantiate every plugin in the chain (sends SetSlotPlugin), then push
        // the song + project_dir + LoadSong to the audio engine.
        h.app.restore_plugin_from_song(&song);
        h.app.song_doc.replace_song(song.clone());
        // headless (frame loop 無し) なので明示的に flush する。 replace_song が epoch を
        // bump しているので flush_song_sync は必ず choreography を実行する。
        h.app.flush_song_sync();
        h.last_loaded_song = Some(song);
    });
    Ok(JsValue::undefined())
}

/// `daw.play()` — start realtime transport in the audio engine (bypasses the
/// GUI play-gating, which assumes the winit loop). Used to reproduce the
/// "played first" state where a synth holds a live voice.
fn daw_play(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_host(|h| {
        let _ = h.bootstrap.audio_tx.send(AudioCommand::Play);
    });
    Ok(JsValue::undefined())
}

/// `daw.stop()` — stop realtime transport.
fn daw_stop(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    with_host(|h| {
        let _ = h.bootstrap.audio_tx.send(AudioCommand::Stop);
    });
    Ok(JsValue::undefined())
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
    let res = with_host(|h| {
        let _ = h
            .bootstrap
            .plugin_tx
            .send(PluginCommand::ReinitAllPlugins);
        h.pump_until(
            |msg| matches!(msg, ChildEvent::Plugin(PluginEvent::PluginsReinitDone)),
            Duration::from_millis(timeout_ms),
        )
    });
    res.map_err(|e| js_native(format!("reinitForExport: {e}")))?;
    Ok(JsValue::undefined())
}

/// `daw.exportWavRange(path, startFrame, endFrame, timeoutMs)` — offline export
/// of a sample-frame range (the cold range, GUI's
/// `AudioCommand::ExportWav { range: Some(..) }`), driven headlessly.
fn daw_export_wav_range(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path_str = String::try_from_js(args.get_or_undefined(0), ctx)?;
    let start = u64::try_from_js(args.get_or_undefined(1), ctx)?;
    let end = u64::try_from_js(args.get_or_undefined(2), ctx)?;
    let timeout_ms = u64::try_from_js(args.get_or_undefined(3), ctx).unwrap_or(120_000);
    let pump_result = with_host(|h| {
        h.drain_pending_for(Duration::from_millis(50));
        let _ = h.bootstrap.audio_tx.send(AudioCommand::ExportWav {
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
        // v29: Song は daw_audio のみが受ける (plugin_host に LoadSong は無い)。
        let cloned = song.clone();
        let _ = h.bootstrap.audio_tx.send(AudioCommand::LoadSong(cloned));
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
        host.app.song_doc.replace_song(song);
        // headless: frame flush が無いので明示 flush (replace_song の epoch bump を拾う)。
        host.app.flush_song_sync();
    });
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
    let json = with_host(|host| {
        let mut song = host.app.song_doc.song().clone();
        let names = song.clip_content_names.clone();
        let rehydrate = |clips: &mut Vec<common::model::Clip>| {
            for clip in clips {
                if let Some(n) = names.get(&clip.content_id) {
                    clip.name = n.clone();
                }
            }
        };
        for track in &mut song.tracks {
            rehydrate(&mut track.clips);
            for lane in &mut track.automation_lanes {
                for clip in &mut lane.clips {
                    if let Some(n) = names.get(&clip.content_id) {
                        clip.name = n.clone();
                    }
                }
            }
        }
        for lane in &mut song.song_lanes {
            for clip in &mut lane.clips {
                if let Some(n) = names.get(&clip.content_id) {
                    clip.name = n.clone();
                }
            }
        }
        serde_json::to_string(&song)
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
    let target: ClipRef = serde_json::from_str(&ref_json)
        .map_err(|e| js_native(format!("clipDisplayLabel: parse: {e}")))?;
    let label = with_host(|host| {
        let song = &host.app.song_doc.song();
        let Some(clip) = song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
        else {
            return String::new();
        };
        crate::widgets::arrangement::view_build::clip_display_label(clip, song).to_string()
    });
    Ok(JsString::from(label.as_str()).into())
}

/// `daw.deviceChain(track_id)` → `host.app.song_doc.song()` の指定トラックの単一デバイス
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
    let json = with_host(|host| {
        let devices: &[common::model::PluginInstance] =
            if track_id == common::model::MASTER_TRACK_ID {
                &host.app.song_doc.song().master_fx_chain
            } else {
                host.app
                    .song_doc.song()
                    .tracks
                    .iter()
                    .find(|t| t.id == track_id)
                    .map(|t| t.devices.as_slice())
                    .unwrap_or(&[])
            };
        let chain: Vec<ChainDevice> = devices
            .iter()
            .map(|d| ChainDevice { plugin_id: d.plugin_id.as_str(), ports: d.ports })
            .collect();
        serde_json::to_string(&chain)
    })
    .map_err(|e| js_native(format!("deviceChain: serialize: {e}")))?;
    Ok(JsString::from(json.as_str()).into())
}

fn daw_set_selection(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_to_string(args, 0, ctx)?;
    let refs: Vec<ClipRef> = serde_json::from_str(&json)
        .map_err(|e| js_native(format!("setSelection: parse: {e}")))?;
    with_host(|host| {
        // ClipRef (index) → stable ClipKey に変換して格納。
        let keys: Vec<common::model::ClipKey> =
            refs.iter().filter_map(|r| host.app.clip_key_of(*r)).collect();
        host.app.selection.selected_clip = keys.last().copied();
        host.app.selection.selected_clips = keys;
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
    with_host(|host| host.app.ui_ephemeral.arrangement_hover_clip = cref);
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
        host.app.ui_ephemeral.arrangement_hover_beat = beat;
        host.app.ui_ephemeral.arrangement_hover_beat_raw = beat;
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
