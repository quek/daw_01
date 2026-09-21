//! one-shot 読み込みベンチ (`--load-bench`)。
//!
//! 同じプラグインを N 本立て、1 本ごとの所要をフェーズ別に、常駐メモリと一緒に出す。
//! 「プロジェクトを開くのが遅い / 重い」がこちら側の何に由来するかを、GUI も
//! オーディオデバイスも通さずに測るための器。`--ara-selftest` / `--editor-selftest` と
//! 同じ使い捨てプロセス方式。
//!
//! ```text
//! daw_plugin_host --load-bench "<path>.vst3" [plugin_id] [n] [flags...]
//! ```
//!
//! flags (順不同):
//! - `noshare` — モジュール共有を切り、1 本ごとに DSO を読み直す
//!   (= [`crate::module_cache`] 導入前の挙動)。共有の効き目は 2 回の差で測る。
//! - `state=<file>` — 各インスタンスにその state を読ませる (= プロジェクトを開く経路)。
//! - `gui` — `gui_is_embed_supported()` を呼ぶ。VST3 ではこれが `createView` =
//!   エディタ実体の生成で、時間もメモリもここで跳ねる。付けた / 付けないの差がそのコスト。
//! - `run=<frames>` — 全インスタンスを立て終えてから、**1 本ずつ順番に** その buffer 長で
//!   `process()` を回し、1 呼び出しの所要を出す。**これが「他のスレッドと取り合っていない
//!   ときの 1 本のコスト」** で、実機の per-plugin 計測 (取り合っている値) と比べるための
//!   対照。差が出たら原因はプラグインではなくスケジューリング側にある。
//! - `notes` — `run` のとき各 instance にノートを 1 つ与えてから回す (無音の
//!   instance は voice を走らせないので、鳴っている状態のコストが測れない)。
//! - `par=<k>` — `run` を **k スレッド同時** で回す (1 スレッド = 1 instance、毎回
//!   barrier で揃える)。IPC も worker pool も engine も通らない純粋な並列で、
//!   「同時に走らせると 1 本あたりが何倍になるか」がプラグイン / マシン側の性質なのか、
//!   こちらの dispatch の性質なのかを分ける。`run=` の逐次値との比が答え。
//! - `rt` — `par` のスレッドを実機と同じ条件 (TIME_CRITICAL + MMCSS "Pro Audio") に置く。
//!   付けない場合は通常優先度。優先度設計そのものの寄与はこの 2 回の差で測る。
//! - `shadow` — `par` の各スレッドに **相方スレッド** を 1 本足す。out-of-process
//!   ホスティングでは 1 プラグインの同時実行に 2 スレッド要る (依頼して完了を待つ
//!   audio runner + `process()` を回す plugin-host worker)。相方は依頼が出ている間
//!   `spin_budget` ぶん回り、間に合わなければ寝る。`shadow` あり / なしの差が
//!   **「完了を待つだけのスレッドがいくら取るか」** の下限になる。
//!
//!   **限界: 実機の runner より大人しい。** 実機の runner は待つだけでなくグラフの手
//!   (sequencer / mixer / 合流) も実行し、仕事を探して queue を見に行く。なのでこの値は
//!   相方のコストを**過小**に出す。実測 (2026-09-21): ここでは 24 並列で +9% だったが、
//!   同じ条件で pair を 32 / 42 に増やした実エンジンは DSP 39.9% → 54.2% / 50.4% と
//!   悪化した (ベンチの外挿では良くなるはずだった)。**方針を決める数字は必ず実エンジン
//!   (`DAW_AUDIO_WORKERS` + `daw_audio::graph::profile`) で取ること。**
//!
//! 実測 (Analog Lab V、2026-09-21、n=8、state 付き):
//!
//! ```text
//! gui なし   490 ms/本   144 MiB/本
//! gui あり  1350 ms/本   274 MiB/本
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;

use crate::module_cache::ModuleCache;
use crate::plugin_instance::{
    HostCallbacks, LoadedPlugin, NoteTransition, TimedNoteEvent, TransportContext, load_plugin,
};
use crate::teardown_plugin;

/// ベンチの振る舞いを決める指定 (`--load-bench` の flag 群)。引数を 1 つずつ渡すと
/// 呼び出しも signature も読めなくなるので束ねる。既定 = 「立てるだけ」。
#[derive(Default, Clone, Copy)]
struct BenchOpts {
    /// `false` = モジュール共有を切る (`noshare`)。
    share: bool,
    /// `gui_is_embed_supported()` を呼ぶ (`gui`)。
    probe_gui: bool,
    /// `process()` を回す buffer 長 (`run=<frames>`)。
    run_frames: Option<u32>,
    /// `run` のとき各 instance にノートを 1 つ与える (`notes`)。
    notes: bool,
    /// `run` を k スレッド同時で回す (`par=<k>`)。
    par: Option<usize>,
    /// `par` のスレッドを TIME_CRITICAL + MMCSS に置く (`rt`)。
    rt: bool,
    /// `par` の各スレッドに相方スレッドを足す (`shadow`)。
    shadow: bool,
}

/// `--load-bench` の argv を解釈して回す。plugin の load は専用スレッドで行う
/// (他の one-shot モードと同じ: プラグインが main スレッドを前提にしても壊れない)。
pub(crate) fn run_from_args() -> Result<()> {
    let path = std::env::args().nth(2).context("--load-bench needs <path>")?;
    let target_id = std::env::args().nth(3).unwrap_or_default();
    let n: usize = std::env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let flags: Vec<String> = std::env::args().skip(5).collect();
    let has = |k: &str| flags.iter().any(|f| f == k);
    let val = |k: &str| flags.iter().find_map(|f| f.strip_prefix(k));
    let opts = BenchOpts {
        share: !has("noshare"),
        probe_gui: has("gui"),
        run_frames: val("run=").and_then(|v| v.parse().ok()),
        notes: has("notes"),
        par: val("par=").and_then(|v| v.parse().ok()),
        rt: has("rt"),
        shadow: has("shadow"),
    };
    let state = match val("state=") {
        Some(f) => Some(std::fs::read(f).with_context(|| format!("reading {f}"))?),
        None => None,
    };
    let joined = std::thread::spawn(move || {
        load_bench(std::path::Path::new(&path), &target_id, n, state.as_deref(), opts)
    })
    .join();
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(e)) => println!("load-bench: ERROR {e:#}"),
        Err(_) => println!("load-bench: PANIC on worker thread"),
    }
    Ok(())
}

/// このプロセスの private commit (= ページファイルに予約したバイト数) と working set。
/// private は「このプロセスだけが持っているメモリ」なので、共有 DLL の image を含まない
/// = プラグインのヒープが何本ぶん積まれたかがそのまま出る。
pub(crate) fn process_memory_mib() -> (f64, f64) {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(0);
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut c).cast::<PROCESS_MEMORY_COUNTERS>(),
            size,
        )
    };
    if ok.is_err() {
        return (f64::NAN, f64::NAN);
    }
    const MIB: f64 = 1024.0 * 1024.0;
    (c.PrivateUsage as f64 / MIB, c.WorkingSetSize as f64 / MIB)
}

/// 同じプラグインを `n` 本立て、1 本ごとの所要をフェーズ別に出す。
///
/// `share = false` のときはインスタンスごとに新しい [`module_cache::ModuleCache`] を作る
/// = DSO を読み直し entry (`InitDll` / `clap_entry.init`) を呼び直す。これが
/// `module_cache` 導入前の挙動で、共有との差がそのまま「モジュールを共有しないと
/// どれだけ損をするか」になる。
fn load_bench(
    path: &std::path::Path,
    target_id: &str,
    n: usize,
    state: Option<&[u8]>,
    opts: BenchOpts,
) -> Result<()> {
    let BenchOpts { share, probe_gui, run_frames, notes, par, rt, shadow } = opts;
    let format = if path.extension().and_then(|e| e.to_str()) == Some("clap") {
        PluginFormat::Clap
    } else {
        PluginFormat::Vst3
    };
    let (base_priv, base_ws) = process_memory_mib();
    println!(
        "load-bench: {} format={format:?} n={n} share={share} state={}B gui={probe_gui} baseline private={base_priv:.0}MiB ws={base_ws:.0}MiB",
        path.display(),
        state.map_or(0, <[u8]>::len)
    );
    println!(
        "inst  load_ms  state_ms  activate_ms  latency_ms  params_ms  gui_ms  total_ms  n_params  private_MiB  ws_MiB"
    );

    let mut shared = ModuleCache::default();
    // 立てた instance は最後まで保持する (drop すると本数に対するメモリの傾きが測れない)。
    let mut kept: Vec<Box<dyn LoadedPlugin>> = Vec::with_capacity(n);
    for i in 0..n {
        let mut fresh = ModuleCache::default();
        let modules = if share { &mut shared } else { &mut fresh };

        let t0 = Instant::now();
        let mut plugin = load_plugin(modules, format, path, target_id, HostCallbacks::noop())
            .with_context(|| format!("load_plugin failed at instance {i}"))?;
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 実経路 (`set_slot_plugin`) と同じ順: state_load は activate より前。
        let ts = Instant::now();
        if let Some(bytes) = state {
            plugin
                .state_load(bytes)
                .with_context(|| format!("state_load failed at instance {i}"))?;
        }
        let state_ms = ts.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        plugin.activate(48_000.0, 64, 1024)?;
        plugin.start_processing()?;
        let activate_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = Instant::now();
        let _latency = plugin.query_latency();
        let latency_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let t3 = Instant::now();
        let params = plugin.enumerate_params();
        let params_ms = t3.elapsed().as_secs_f64() * 1000.0;

        let t4 = Instant::now();
        if probe_gui {
            let _ = plugin.gui_is_embed_supported();
        }
        let gui_ms = t4.elapsed().as_secs_f64() * 1000.0;

        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let (pv, ws) = process_memory_mib();
        println!(
            "{:4}  {load_ms:7.0}  {state_ms:8.0}  {activate_ms:11.0}  {latency_ms:10.0}  {params_ms:9.0}  {gui_ms:6.0}  {total_ms:8.0}  {:8}  {:11.0}  {ws:6.0}",
            i + 1,
            params.len(),
            pv,
        );
        kept.push(plugin);
    }
    let (pv, ws) = process_memory_mib();
    #[allow(clippy::cast_precision_loss)]
    let per = (pv - base_priv) / n as f64;
    println!(
        "load-bench: {n} instances  private {base_priv:.0} -> {pv:.0} MiB ({per:.0} MiB/instance)  ws {base_ws:.0} -> {ws:.0} MiB"
    );
    if let Some(frames) = run_frames {
        match par {
            Some(k) => {
                let k = k.min(kept.len());
                run_parallel_bench(&mut kept, frames, notes, k, rt, shadow);
            }
            None => run_process_bench(&mut kept, frames, notes),
        }
    }

    // teardown は測らない (ここで落ちてもベンチの数字は出し切っている)。
    for plugin in kept.drain(..) {
        teardown_plugin(plugin);
    }
    Ok(())
}

/// 立て終えた instance を **1 本ずつ順番に** `process()` して、1 呼び出しの所要を出す。
///
/// 実機 (`PluginMetricsPlane` の per-plugin μs) は 30 本を同時に走らせた値なので、
/// コア・SMT・スケジューラの取り合いが入る。ここは 1 本しか走らせないので、その取り合いが
/// 無いときの素のコストが出る。**2 つの差が「並列化で払っている税」** になる。
fn run_process_bench(plugins: &mut [Box<dyn LoadedPlugin>], frames: u32, notes: bool) {
    const WARMUP: usize = 20;
    const MEASURED: usize = 200;
    let n = frames as usize;
    let silence = vec![0.0_f32; n];
    let input: Vec<&[f32]> = vec![&silence, &silence];
    let transport = bench_transport();
    let note_on = [TimedNoteEvent {
        time: 0,
        event: NoteTransition::On { note_id: 1, key: 60, velocity: 0.8 },
    }];

    let budget_us = f64::from(frames) / 48_000.0 * 1e6;
    println!(
        "process bench: frames={frames} (budget {:.2}ms) warmup={WARMUP} measured={MEASURED} notes={notes}",
        budget_us / 1000.0
    );
    println!("inst  median_us  p95_us  max_us  load_pct");
    let mut medians: Vec<f64> = Vec::with_capacity(plugins.len());
    for (i, plugin) in plugins.iter_mut().enumerate() {
        // SAFETY: worker pool を開いていないので audio half を触るのはこのスレッドだけ
        // (`AudioHalf::get` の quiesced-window 契約を、そもそも並行が無いことで満たす)。
        let audio = plugin.audio_half();
        let half = unsafe { audio.get() };
        let mut us: Vec<u128> = Vec::with_capacity(MEASURED);
        for iter in 0..(WARMUP + MEASURED) {
            let ev: &[TimedNoteEvent] = if notes && iter == 0 { &note_on } else { &[] };
            let t = Instant::now();
            let _ = half.process(frames, ev, &[], &input, &[], &transport);
            if iter >= WARMUP {
                us.push(t.elapsed().as_micros());
            }
        }
        us.sort_unstable();
        #[allow(clippy::cast_precision_loss)]
        let med = us[us.len() / 2] as f64;
        medians.push(med);
        println!(
            "{:4}  {med:9.0}  {:6}  {:6}  {:7.1}",
            i + 1,
            us[us.len() * 95 / 100],
            us[us.len() - 1],
            100.0 * med / budget_us,
        );
    }
    let sum: f64 = medians.iter().sum();
    #[allow(clippy::cast_precision_loss)]
    let count = medians.len() as f64;
    println!(
        "process bench: {count:.0} instances  median/inst={:.0}us ({:.1}% of budget)  serial sum={:.2}ms ({:.0}% of budget)",
        sum / count,
        100.0 * (sum / count) / budget_us,
        sum / 1000.0,
        100.0 * sum / budget_us,
    );
}

/// `k` スレッドで **同時に** `process()` を回し、1 呼び出しの所要を出す。
///
/// 1 スレッド = 1 instance を固定で持ち、毎回 barrier で揃えてから呼ぶ (= 実機の
/// 「1 buffer ぶんを一斉に処理する」形)。engine も worker pool も IPC も通らないので、
/// [`run_process_bench`] の逐次値との比が **並列にしたこと自体のコスト**になる。
fn run_parallel_bench(
    plugins: &mut [Box<dyn LoadedPlugin>],
    frames: u32,
    notes: bool,
    k: usize,
    rt: bool,
    shadow: bool,
) {
    use std::sync::Barrier;

    let budget_us = f64::from(frames) / 48_000.0 * 1e6;
    println!(
        "parallel bench: threads={k} frames={frames} (budget {:.2}ms) rt={rt} notes={notes} shadow={shadow}",
        budget_us / 1000.0
    );

    let halves: Vec<_> = plugins.iter().take(k).map(|p| p.audio_half()).collect();
    let barrier = Barrier::new(k);
    let results = std::sync::Mutex::new(Vec::<(usize, u128, u128)>::new());
    // worker ごとの札。`busy` = いま `process()` の中 (= 相方が完了を待っている)、
    // `done` = この worker は全計測を終えた (= 相方も抜けてよい)。
    let pairs: Vec<ShadowFlags> = (0..k).map(|_| ShadowFlags::default()).collect();

    std::thread::scope(|scope| {
        for (i, half) in halves.iter().enumerate() {
            let barrier = &barrier;
            let results = &results;
            let flags = &pairs[i];
            scope.spawn(move || {
                let us = parallel_worker(half, frames, notes, rt, barrier, flags);
                flags.done.store(true, Ordering::Release);
                results.lock().unwrap().push((i, us.0, us.1));
            });
            if shadow {
                let flags = &pairs[i];
                scope.spawn(move || shadow_runner(frames, rt, flags));
            }
        }
    });

    let mut r = results.into_inner().unwrap();
    r.sort_unstable();
    #[allow(clippy::cast_precision_loss)]
    let mut meds: Vec<f64> = r.iter().map(|(_, m, _)| *m as f64).collect();
    let sum: f64 = meds.iter().sum();
    #[allow(clippy::cast_precision_loss)]
    let count = meds.len() as f64;
    meds.sort_by(f64::total_cmp);
    println!(
        "parallel bench: {count:.0} threads  median/inst={:.0}us (min {:.0} / max {:.0})  {:.1}% of budget  sum={:.2}ms",
        sum / count,
        meds[0],
        meds[meds.len() - 1],
        100.0 * (sum / count) / budget_us,
        sum / 1000.0,
    );
}

/// [`run_parallel_bench`] の 1 スレッドぶん。返り値は (中央値 μs, 最大 μs)。
fn parallel_worker(
    half: &std::sync::Arc<crate::plugin_instance::AudioHalf>,
    frames: u32,
    notes: bool,
    rt: bool,
    barrier: &std::sync::Barrier,
    flags: &ShadowFlags,
) -> (u128, u128) {
    const WARMUP: usize = 20;
    const MEASURED: usize = 200;

    if rt {
        // 実機の worker と同じ条件に置く。
        unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }
    let _mmcss = if rt { common::mmcss::join_pro_audio() } else { None };

    let n = frames as usize;
    let silence = vec![0.0_f32; n];
    let input: Vec<&[f32]> = vec![&silence, &silence];
    let transport = bench_transport();
    let note_on = [TimedNoteEvent {
        time: 0,
        event: NoteTransition::On { note_id: 1, key: 60, velocity: 0.8 },
    }];
    // SAFETY: この instance の audio half を触るのはこのスレッドだけ
    // (1 スレッド 1 instance、worker pool は開いていない)。
    let plugin = unsafe { half.get() };
    let mut us: Vec<u128> = Vec::with_capacity(MEASURED);
    for iter in 0..(WARMUP + MEASURED) {
        let ev: &[TimedNoteEvent] = if notes && iter == 0 { &note_on } else { &[] };
        barrier.wait();
        let t = Instant::now();
        // 相方が「依頼が出ている」と見る窓 = 実機の dispatch 窓。
        flags.busy.store(true, Ordering::Release);
        let _ = plugin.process(frames, ev, &[], &input, &[], &transport);
        flags.busy.store(false, Ordering::Release);
        let e = t.elapsed().as_micros();
        if iter >= WARMUP {
            us.push(e);
        }
    }
    us.sort_unstable();
    (us[us.len() / 2], us[us.len() - 1])
}

/// worker と相方の間の札。
#[derive(Default)]
struct ShadowFlags {
    /// worker が `process()` の中にいる (= 相方は完了を待っている)。
    busy: AtomicBool,
    /// worker が全計測を終えた (= 相方も抜けてよい)。
    done: AtomicBool,
}

/// 実機の audio runner と同じ待ち方をする相方スレッド。
///
/// 依頼 (`busy`) が出たら [`common::worker_bridge::spin_budget`] ぶん回り、間に合わ
/// なければ寝る。**待つだけで 1 コアを占有はしない**が、スレッドは実在してスケジューラの
/// 取り合いには入る — その寄与を測るためだけに居る。
fn shadow_runner(frames: u32, rt: bool, flags: &ShadowFlags) {
    if rt {
        unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }
    let _mmcss = if rt { common::mmcss::join_pro_audio() } else { None };
    let spin = common::worker_bridge::spin_budget(frames, 48_000);
    while !flags.done.load(Ordering::Acquire) {
        if !flags.busy.load(Ordering::Acquire) {
            // 次の依頼を待つ (実機の runner も次の job まで寝ている)。
            std::thread::sleep(std::time::Duration::from_micros(50));
            continue;
        }
        // 依頼が出ている間: まず回って待つ。
        let until = Instant::now() + spin;
        while flags.busy.load(Ordering::Acquire) && Instant::now() < until {
            std::hint::spin_loop();
        }
        // 間に合わなければ寝て待つ (実機は event、ここは同じ「寝る」効果の sleep)。
        while flags.busy.load(Ordering::Acquire) && !flags.done.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }
}

/// ベンチ共通の transport (120 BPM / 4-4 / 再生中 / 曲頭)。
fn bench_transport() -> TransportContext {
    TransportContext {
        bpm: 120.0,
        sample_rate: 48_000,
        song_pos_beats: 0.0,
        tsig_num: 4,
        tsig_denom: 4,
        is_playing: true,
        is_looping: false,
        loop_start_beats: 0.0,
        loop_end_beats: 0.0,
        row: common::process_data::RowTransport { pos_beats: 0.0, ..Default::default() },
        pin_to_song: false,
    }
}
