//! r.md #129 §15.3: 内蔵 device の op (`ChainOp::Native`) の RT 実行。
//! T2 状態の引き継ぎ / T3 bypass の crossfade / T5 SC の staging / T6 group の変調 / T7 SC Listen /
//! T12 無確保 (`--features rt-assert`)。

use std::collections::{HashMap, HashSet};

use common::dsp::BiquadState;
use common::mod_plane::ModTickPlaneRef;
use common::model::{
    AudioTap, AutomationLane, AutomationTarget, AuxInputRoute, CompParam, CompSettings, Device, EqBand, EqParam,
    LoopRegion, ModRouting, NativeDevice, NativeKind, NativeParamId, NativeParams, Parallel, ParallelChain,
    PluginInstance, Polarity, Song, TapPoint, TapSource, Track, TrackBuiltinParam,
};

use super::*;
use crate::engine::PluginRefs;
use crate::graph::{
    ChainProgram, DeviceLatencies, NodeOp, ProgramCtx, Schedule, build_program, compile_schedule_for_test,
    execute_schedule_post_dispatch, run_chain_program,
};
use crate::launcher::{RowSourceTable, TrackRows};
use crate::mixer::{MAX_EVENTS, TrackScratch};
use crate::mod_tick::FollowerDrive;
use crate::native_dsp::{NativeBlock, NativeDsp};

const SR: u32 = 48_000;

/// `Track { .., ..Track::default() }` は common の private field で書けない (E0451)。
fn track(f: impl FnOnce(&mut Track)) -> Track {
    let mut t = Track::default();
    f(&mut t);
    t
}

fn comp(id: u64, s: CompSettings) -> NativeDevice {
    NativeDevice { params: NativeParams::Comp(s), ..NativeDevice::new_added(NativeKind::Comp, id, 1) }
}

/// 深く潰す Comp (−40 dB / 20:1 / 0.1 ms)。
fn hard_comp(id: u64) -> NativeDevice {
    comp(id, CompSettings { threshold_db: -40.0, ratio: 20.0, attack_ms: 0.1, release_ms: 50.0, ..CompSettings::default() })
}

fn eq_boost(id: u64) -> NativeDevice {
    let mut d = NativeDevice::new_added(NativeKind::Eq, id, 1);
    d.set_param(NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain }, 12.0);
    d
}

fn with_sc(mut d: NativeDevice, source: TapSource, point: TapPoint) -> NativeDevice {
    d.aux_input = Some(AuxInputRoute { tap: AudioTap::new(source, point) });
    d
}

fn sine(n: usize, start: usize, hz: f32, amp: f32) -> Vec<f32> {
    (0..n).map(|i| amp * (std::f32::consts::TAU * hz * (start + i) as f32 / SR as f32).sin()).collect()
}

fn ramp(n: usize, k: f32) -> Vec<f32> {
    (0..n).map(|i| k * (i as f32 + 1.0)).collect()
}

struct Env {
    refs: PluginRefs,
    rec: HashSet<(u32, AutomationTarget)>,
}

impl Env {
    fn new() -> Self {
        Self { refs: HashMap::new(), rec: HashSet::new() }
    }
}

fn ctx<'a>(env: &'a Env, song: &'a Song, devices: &'a [Device], n: usize) -> ProgramCtx<'a> {
    ProgramCtx {
        song: Some(song),
        plugin_refs: &env.refs,
        worker_sync: None,
        sample_rate: SR,
        frames: n as u32,
        playing: true,
        current_bpm: 120.0,
        playhead_beats: 0.0,
        loop_region: LoopRegion::default(),
        recording_lanes: &env.rec,
        mod_plane: ModTickPlaneRef::default(),
        rows: TrackRows::default(),
        own_pre_fx: None,
        native: NativeIo::default(),
        owner_devices: devices,
        owner_stores: (&[], &[]),
    }
}

fn build(devices: &[Device], track_id: u32) -> ChainProgram {
    build_program(devices, track_id, None, &DeviceLatencies::new(), &HashSet::new()).program
}

fn run(program: &mut ChainProgram, l: &mut [f32], r: &mut [f32], ctx: &ProgramCtx<'_>) {
    let (mut a, mut b) = (Vec::with_capacity(MAX_EVENTS), Vec::with_capacity(MAX_EVENTS));
    let len = program.ops.len();
    run_chain_program(program, 0..len, l, r, &mut a, &mut b, ctx);
}

fn slot_of(p: &ChainProgram, id: u64) -> usize {
    p.natives.iter().position(|ns| ns.device_id == id).expect("native slot")
}

fn scratches(n: usize) -> Vec<TrackScratch> {
    (0..n).map(|_| TrackScratch::new()).collect()
}

/// pass 2 (`execute_schedule_post_dispatch`) を 1 buffer 走らせる。pass 1 の出力は呼び側が scratch に置く。
fn post_dispatch(
    sched: &mut Schedule,
    scratch: &mut [TrackScratch],
    song: &Song,
    n: usize,
    rec: &HashSet<(u32, AutomationTarget)>,
    mod_plane: ModTickPlaneRef<'_>,
    native: NativeIo<'_>,
) {
    let refs: PluginRefs = HashMap::new();
    let (mut ml, mut mr) = (vec![0.0; n], vec![0.0; n]);
    execute_schedule_post_dispatch(
        sched,
        scratch,
        &mut ml,
        &mut mr,
        n,
        song,
        &refs,
        None,
        SR,
        n as u32,
        true,
        false,
        rec,
        120.0,
        0.0,
        LoopRegion::default(),
        mod_plane,
        FollowerDrive::default(),
        &RowSourceTable::default(),
        native,
    );
}

/// T2: device id で状態を引き継ぐ。深く潰れている Comp の前に plugin と EQ を差し込んで compile し直しても、
/// 次の block の GR は連続する。同じ id でも種類が違えば新品の状態から始まる。
#[test]
fn state_follows_the_device_id_across_a_reordering_recompile() {
    let env = Env::new();
    let song = Song::default();
    let n = 512;
    let slow = comp(
        10,
        CompSettings { threshold_db: -30.0, ratio: 10.0, attack_ms: 50.0, release_ms: 500.0, ..CompSettings::default() },
    );
    let block = |p: &mut ChainProgram, devices: &[Device], b: usize| {
        let mut l = sine(n, b * n, 220.0, 0.9);
        let mut r = l.clone();
        run(p, &mut l, &mut r, &ctx(&env, &song, devices, n));
        (l, r)
    };
    let before = vec![Device::Native(slow)];
    let (mut old, mut uninterrupted) = (build(&before, 1), build(&before, 1));
    for b in 0..20 {
        block(&mut old, &before, b);
        block(&mut uninterrupted, &before, b);
    }
    assert!(old.natives[0].gr_db < -10.0, "深く潰れている: {}", old.natives[0].gr_db);
    // 比べる相手 = compile し直さずに走り続けた場合の次の block の GR。
    block(&mut uninterrupted, &before, 20);
    let settled = uninterrupted.natives[0].gr_db;

    let plugin = Device::Plugin(PluginInstance {
        id: 99,
        ..PluginInstance::new("p".into(), common::plugin_format::PluginFormat::Clap)
    });
    let after = vec![Device::Native(NativeDevice::new_added(NativeKind::Eq, 11, 1)), plugin, Device::Native(slow)];
    let mut adopted = build(&after, 1);
    adopted.adopt_state_from(&mut old);
    block(&mut adopted, &after, 20);
    let gr = adopted.natives[slot_of(&adopted, 10)].gr_db;
    assert!((gr - settled).abs() < 0.05, "引き継いだ GR が連続する: {gr} vs {settled}");
    let mut fresh = build(&after, 1);
    block(&mut fresh, &after, 20);
    let fresh_gr = fresh.natives[slot_of(&fresh, 10)].gr_db;
    assert!((fresh_gr - settled).abs() > 1.0, "引き継がなければ無音の状態から始まる: {fresh_gr}");

    // 同じ id でも種類が違えば引き継がない = 新品の EQ と同じ出力。
    let other = vec![Device::Native(eq_boost(10))];
    let mut swapped = build(&other, 1);
    swapped.adopt_state_from(&mut old);
    let mut reference = build(&other, 1);
    assert_eq!(block(&mut swapped, &other, 21), block(&mut reference, &other, 21));
    assert_eq!(swapped.natives[0].gr_db, 0.0);
}

/// T3: bypass の切り替えは crossfade (段差なし)。落ち着いた bypass は入力とビット一致で GR 0。
/// 再開直後の出力は、reset した単独の DSP を wet としたフェードと一致する (古いフィルタ状態から再開しない)。
#[test]
fn bypass_crossfades_then_settles_bit_exact_and_restarts_from_a_reset_dsp() {
    let env = Env::new();
    let song = Song::default();
    let n = 64;
    let on_dev = hard_comp(10);
    let on = vec![Device::Native(on_dev)];
    let off = vec![Device::Native(NativeDevice { bypassed: true, ..on_dev })];
    let mut p = build(&on, 1);
    let (mut input, mut output, mut grs) = (Vec::new(), Vec::new(), Vec::new());
    // 40 block ON → 40 block OFF → 40 block ON (1 block = 64 frame、フェード 5 ms = 240 frame)。
    for b in 0..120 {
        let devices = if (40..80).contains(&b) { &off } else { &on };
        let x = sine(n, b * n, 100.0, 0.5);
        let (mut l, mut r) = (x.clone(), x.clone());
        run(&mut p, &mut l, &mut r, &ctx(&env, &song, devices, n));
        if b == 80 {
            let mut fresh = NativeDsp::new(NativeKind::Comp);
            let (mut wl, mut wr) = (x.clone(), x.clone());
            fresh.process(
                &on_dev.params,
                NativeBlock { l: &mut wl, r: &mut wr, n, sample_rate: SR as f32, sidechain: None, listen_out: None },
            );
            let to = n as f32 / (NATIVE_BYPASS_FADE_MS * 0.001 * SR as f32);
            for i in 0..n {
                let w = to * ((i + 1) as f32 / n as f32);
                let want = x[i] + (wl[i] - x[i]) * w;
                assert!((l[i] - want).abs() < 1e-6, "再開直後 i={i}: {} vs {want}", l[i]);
            }
        }
        input.extend_from_slice(&x);
        output.extend_from_slice(&l);
        grs.push(p.natives[0].gr_db);
    }
    // 切り替えの前後で段差が出ない (先頭 4 block は起動時のアタックなので除く)。
    let natural = 0.5 * std::f32::consts::TAU * 100.0 / SR as f32;
    for i in (4 * n)..output.len() - 1 {
        let d = (output[i + 1] - output[i]).abs();
        assert!(d <= natural + 0.02, "i={i} (block {}): 段差 {d}", i / n);
    }
    for b in 50..80 {
        assert_eq!(&output[b * n..(b + 1) * n], &input[b * n..(b + 1) * n], "落ち着いた bypass は素通し (block {b})");
        assert_eq!(grs[b], 0.0);
    }
    assert!(grs[100] < -10.0, "再開後は効いている: {}", grs[100]);
}

fn native_taps(s: &Schedule) -> Vec<(BufRef, u32, u32)> {
    s.nodes
        .iter()
        .filter_map(|op| match op {
            NodeOp::NativeSidechainTap { src, owner, native_slot } => Some((*src, *owner, *native_slot)),
            _ => None,
        })
        .collect()
}

fn parallel(id: u64, chain_id: u64, devices: Vec<Device>) -> Device {
    Device::Parallel(Parallel {
        id,
        chains: vec![ParallelChain { id: chain_id, devices, ..ParallelChain::new("c") }],
        ..Parallel::new()
    })
}

/// T5: `NativeSidechainTap` の staging は 3 通りの借用 (scratch / 同じ program の chain / 別 program、master を
/// 含む) で正しいバッファを写す。自トラック Pre-FX は tap を出さず `OwnPreFx`、自トラックの PostFx は feedback なので
/// `None`、bypass 中の Parallel の中の native は op が無いので tap も無い。
#[test]
fn native_sidechain_is_staged_from_scratch_the_same_program_and_other_programs() {
    let n = 32;
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = vec![
                    parallel(50, 51, vec![]),
                    Device::Native(with_sc(hard_comp(20), TapSource::Chain(51), TapPoint::PostFx)),
                ];
            }),
            track(|t| {
                t.id = 2;
                let mut off = Parallel::new();
                off.id = 70;
                off.chains[0].id = 71;
                off.chains[0].devices = vec![Device::Native(with_sc(hard_comp(26), TapSource::Track(1), TapPoint::PostFader))];
                off.bypassed = true;
                t.devices = vec![
                    Device::Native(with_sc(hard_comp(21), TapSource::Track(1), TapPoint::PostFader)),
                    Device::Native(with_sc(hard_comp(22), TapSource::Chain(51), TapPoint::PostFader)),
                    Device::Native(with_sc(hard_comp(23), TapSource::Chain(61), TapPoint::PostFx)),
                    Device::Native(with_sc(hard_comp(24), TapSource::Track(2), TapPoint::PreFx)),
                    Device::Native(with_sc(hard_comp(25), TapSource::Track(2), TapPoint::PostFx)),
                    Device::Parallel(off),
                ];
            }),
        ],
        master_fx_chain: vec![
            parallel(60, 61, vec![]),
            Device::Native(with_sc(NativeDevice::new_added(NativeKind::BusComp, 30, 1), TapSource::Chain(51), TapPoint::PreFx)),
        ],
        ..Song::default()
    };
    let mut sched = compile_schedule_for_test(&song, SR, 256).expect("compile");
    let taps = native_taps(&sched);
    assert_eq!(taps.len(), 5, "20 / 21 / 22 / 23 / 30 の 5 本: {taps:?}");
    let t2 = &sched.track_programs[1];
    assert_eq!(t2.natives[slot_of(t2, 24)].sc_mode, ScMode::OwnPreFx);
    assert!(t2.natives[slot_of(t2, 24)].sc.is_none());
    assert_eq!(t2.natives[slot_of(t2, 25)].sc_mode, ScMode::None, "自トラックの PostFx は feedback");
    assert!(!t2.natives.iter().any(|ns| ns.device_id == 26), "bypass 中の Parallel の中には op も slot も無い");

    let mut scratch = scratches(2);
    scratch[0].track_l[..n].copy_from_slice(&ramp(n, 1.0));
    scratch[0].track_r[..n].copy_from_slice(&ramp(n, -1.0));
    let p1 = &mut sched.track_programs[0];
    p1.chains[0].post_fx_l[..n].copy_from_slice(&ramp(n, 2.0));
    p1.chains[0].post_fx_r[..n].copy_from_slice(&ramp(n, -2.0));
    p1.chains[0].post_fader_l[..n].copy_from_slice(&ramp(n, 3.0));
    p1.chains[0].post_fader_r[..n].copy_from_slice(&ramp(n, -3.0));
    p1.parallels[0].in_l[..n].copy_from_slice(&ramp(n, 4.0));
    p1.parallels[0].in_r[..n].copy_from_slice(&ramp(n, -4.0));
    sched.master_program.chains[0].post_fx_l[..n].copy_from_slice(&ramp(n, 5.0));
    sched.master_program.chains[0].post_fx_r[..n].copy_from_slice(&ramp(n, -5.0));
    for (src, owner, slot) in &taps {
        stage_native_sidechain(&scratch, &mut sched.track_programs, &mut sched.master_program, *src, *owner, *slot, n);
    }
    let staged = |p: &ChainProgram, id: u64| {
        let ns = &p.natives[slot_of(p, id)];
        assert_eq!(ns.sc_mode, ScMode::Staged, "device {id}");
        let (l, r) = ns.sc.as_ref().expect("受け皿").signal(n);
        (l.to_vec(), r.to_vec())
    };
    let want = |k: f32| (ramp(n, k), ramp(n, -k));
    assert_eq!(staged(&sched.track_programs[0], 20), want(2.0), "同じ program の chain");
    assert_eq!(staged(&sched.track_programs[1], 21), want(1.0), "scratch");
    assert_eq!(staged(&sched.track_programs[1], 22), want(3.0), "別 track の program");
    assert_eq!(staged(&sched.track_programs[1], 23), want(5.0), "master の program から track へ");
    assert_eq!(staged(&sched.master_program, 30), want(4.0), "track の program から master へ");
    // 消費側は staging した長さまで読む (足りない分は DSP が 0 とみなす)。
    let ns = &sched.track_programs[1].natives[slot_of(&sched.track_programs[1], 21)];
    assert_eq!(ns.sc.as_ref().unwrap().signal(n * 4).0.len(), n);
}

/// T5 (続き): post-dispatch を通して、group の native Comp が外部サイドチェインの音で検出する。
#[test]
fn a_group_native_comp_detects_its_external_sidechain_in_pass_two() {
    let rec = HashSet::new();
    let n = 256;
    for wired in [false, true] {
        let c = hard_comp(40);
        let song = Song {
            tracks: vec![
                track(|t| t.id = 1),
                track(|t| {
                    t.id = 2;
                    t.devices = vec![Device::Native(if wired {
                        with_sc(c, TapSource::Track(1), TapPoint::PostFader)
                    } else {
                        c
                    })];
                }),
                track(|t| {
                    t.id = 3;
                    t.parent_group_id = Some(2);
                }),
            ],
            ..Song::default()
        };
        let mut sched = compile_schedule_for_test(&song, SR, n as u32).expect("compile");
        let mut scratch = scratches(3);
        let loud = sine(n, 0, 440.0, 0.9);
        let quiet = sine(n, 0, 440.0, 0.005);
        scratch[0].track_l[..n].copy_from_slice(&loud);
        scratch[0].track_r[..n].copy_from_slice(&loud);
        scratch[2].track_l[..n].copy_from_slice(&quiet);
        scratch[2].track_r[..n].copy_from_slice(&quiet);
        post_dispatch(&mut sched, &mut scratch, &song, n, &rec, ModTickPlaneRef::default(), NativeIo::default());
        let gr = sched.track_programs[1].natives[0].gr_db;
        if wired {
            assert!(gr < -20.0, "外部 SC (大きい音) で検出する: {gr}");
        } else {
            assert!(gr > -0.5, "自分の入力 (小さい音) では潰れない: {gr}");
        }
    }
}

/// group G (id 2、scratch 1) と子 (id 3、scratch 2)。G に組み込み Comp (id 40)。
fn group_song(comp_dev: NativeDevice, lanes: Vec<AutomationLane>, routings: Vec<ModRouting>) -> Song {
    Song {
        tracks: vec![
            track(|t| {
                t.id = 2;
                t.devices = vec![Device::Native(NativeDevice { builtin: true, ..comp_dev })];
                t.automation_lanes = lanes;
                t.mod_routings = routings;
            }),
            track(|t| {
                t.id = 3;
                t.parent_group_id = Some(2);
            }),
        ],
        ..Song::default()
    }
}

/// G を `blocks` 回走らせ、最後の block の (Comp の GR, G の出力ピーク) を返す。
fn run_group(song: &Song, rec: &HashSet<(u32, AutomationTarget)>, plane: ModTickPlaneRef<'_>, blocks: usize) -> (f32, f32) {
    let n = 256;
    let mut sched = compile_schedule_for_test(song, SR, n as u32).expect("compile");
    let mut scratch = scratches(2);
    for b in 0..blocks {
        let x = sine(n, b * n, 440.0, 0.5);
        scratch[1].track_l[..n].copy_from_slice(&x);
        scratch[1].track_r[..n].copy_from_slice(&x);
        post_dispatch(&mut sched, &mut scratch, song, n, rec, plane, NativeIo::default());
    }
    let peak = scratch[0].track_l[..n].iter().fold(0.0f32, |m, v| m.max(v.abs()));
    (sched.track_programs[0].natives[0].gr_db, peak)
}

fn routing(target: AutomationTarget, depth: f32) -> ModRouting {
    ModRouting { id: 1, target, source_id: 7, depth, polarity: Polarity::Unipolar, enabled: true }
}

/// T6: group / return の pass 2 でも、組み込み Comp のパラメーターと volume に変調が効く (§18-A)。
/// 録音中のレーンは解決しない。On のレーンで実効の ON / OFF が切り替わる。
#[test]
fn group_builtins_and_volume_follow_modulation_lanes_and_on_automation() {
    let none = HashSet::new();
    let plane = ModTickPlaneRef::new(&[7], &[1.0], common::mod_graph::MOD_TICK_FRAMES);
    let thr = AutomationTarget::NativeParam { device_id: 40, param: NativeParamId::Comp(CompParam::Threshold) };
    let open = comp(40, CompSettings { threshold_db: 0.0, ratio: 20.0, attack_ms: 0.1, ..CompSettings::default() });

    // Threshold への変調 (正規化 −1 = −60 dB)。
    let s = group_song(open, vec![], vec![routing(thr.clone(), -1.0)]);
    assert!(run_group(&s, &none, plane, 1).0 < -20.0, "変調で閾値が下がって潰れる");
    assert!(run_group(&s, &none, ModTickPlaneRef::default(), 1).0 > -0.5, "変調が無ければ潰れない");

    // volume への変調 (正規化 0.5 − 0.5 = 0 = 無音)。
    let vol = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
    let s = group_song(open, vec![], vec![routing(vol, -0.5)]);
    assert!(run_group(&s, &none, plane, 1).1 < 1e-4, "group の volume にも変調が効く");
    assert!(run_group(&s, &none, ModTickPlaneRef::default(), 1).1 > 0.1);

    // レーン (値 −60 dB)。録音中は解決しない。
    let s = group_song(open, vec![AutomationLane::new(thr.clone(), -60.0)], vec![]);
    assert!(run_group(&s, &none, ModTickPlaneRef::default(), 1).0 < -20.0, "レーンの値で潰れる");
    let recording: HashSet<_> = [(2u32, thr)].into();
    assert!(run_group(&s, &recording, ModTickPlaneRef::default(), 1).0 > -0.5, "録音中は静的な値");

    // On のレーン: 静的には bypass でもレーンで ON (フェードが終わる 2 block 目以降)、その逆も。
    let on = AutomationTarget::NativeParam { device_id: 40, param: NativeParamId::On(NativeKind::Comp) };
    let deep = comp(40, CompSettings { threshold_db: -60.0, ratio: 20.0, attack_ms: 0.1, ..CompSettings::default() });
    let s = group_song(NativeDevice { bypassed: true, ..deep }, vec![AutomationLane::new(on.clone(), 1.0)], vec![]);
    assert!(run_group(&s, &none, ModTickPlaneRef::default(), 2).0 < -20.0, "On レーンで有効になる");
    let s = group_song(deep, vec![AutomationLane::new(on, 0.0)], vec![]);
    assert_eq!(run_group(&s, &none, ModTickPlaneRef::default(), 2).0, 0.0, "On レーンで無効になる");
}

/// T7 (K25d): Listen 中はチェーン出力 = Listen した Comp の検出信号 (SC フィルタ後)。後段の device (EQ) も
/// Comp も普通に走っていて、Listen を外した次の block からは Listen していない場合とビット一致する。
/// Listen の宛先が EQ / Comp が bypass / 書き出し (`NativeIo::default()`) なら出力は変わらない。
#[test]
fn sc_listen_replaces_the_chain_output_with_the_detection_signal_and_keeps_later_devices_running() {
    let env = Env::new();
    let song = Song::default();
    let n = 128;
    let settings =
        CompSettings { sc_freq_hz: 1_000.0, threshold_db: -30.0, ratio: 8.0, attack_ms: 1.0, ..CompSettings::default() };
    let devices = vec![Device::Native(comp(10, settings)), Device::Native(eq_boost(11))];
    let listen = |id| NativeIo { sc_listen: id, scopes: None };
    let block = |p: &mut ChainProgram, devices: &[Device], io: NativeIo<'_>, b: usize| {
        let x = sine(n, b * n, 700.0, 0.8);
        let (mut l, mut r) = (x.clone(), x);
        run(p, &mut l, &mut r, &ProgramCtx { native: io, ..ctx(&env, &song, devices, n) });
        apply_listen_override(p, &mut l, &mut r, n);
        (l, r)
    };
    let sc = common::dsp::sc_filter(&settings, SR as f32).expect("SC フィルタ");
    let mut det = BiquadState::default();
    let (mut listened, mut reference) = (build(&devices, 1), build(&devices, 1));
    for b in 0..8 {
        let io = if b < 6 { listen(10) } else { NativeIo::default() };
        let got = block(&mut listened, &devices, io, b);
        let normal = block(&mut reference, &devices, NativeIo::default(), b);
        if b < 6 {
            let x = sine(n, b * n, 700.0, 0.8);
            for (i, v) in x.iter().enumerate() {
                let want = det.process(&sc, *v);
                assert!((got.0[i] - want).abs() < 1e-6 && (got.1[i] - want).abs() < 1e-6, "b={b} i={i}");
            }
            assert_ne!(got, normal, "Listen 中は普段の出力ではない");
        } else {
            assert_eq!(got, normal, "Listen 中も Comp と EQ の状態は進んでいる (b={b})");
        }
    }

    let (mut a, mut b) = (build(&devices, 1), build(&devices, 1));
    assert_eq!(block(&mut a, &devices, listen(11), 0), block(&mut b, &devices, NativeIo::default(), 0), "EQ の id");
    let off = vec![Device::Native(NativeDevice { bypassed: true, ..comp(10, settings) }), Device::Native(eq_boost(11))];
    let (mut a, mut b) = (build(&off, 1), build(&off, 1));
    assert_eq!(block(&mut a, &off, listen(10), 0), block(&mut b, &off, NativeIo::default(), 0), "bypass 中の Comp");
}

/// T7 (続き): 置換は group の PostFx 点 (フェーダーの前) で行われる。Listen 中の group 出力は、SC フィルタ OFF の
/// Comp なら「device が 1 つも無い group」の出力とビット一致する (後段の EQ の音は出ない)。
#[test]
fn sc_listen_on_a_group_replaces_its_post_fx_point() {
    let rec = HashSet::new();
    let n = 128;
    let render = |devices: Vec<Device>, io: NativeIo<'_>| {
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 2;
                    t.devices = devices;
                }),
                track(|t| {
                    t.id = 3;
                    t.parent_group_id = Some(2);
                }),
            ],
            ..Song::default()
        };
        let mut sched = compile_schedule_for_test(&song, SR, n as u32).expect("compile");
        let mut scratch = scratches(2);
        let x = sine(n, 0, 300.0, 0.6);
        scratch[1].track_l[..n].copy_from_slice(&x);
        scratch[1].track_r[..n].copy_from_slice(&x);
        post_dispatch(&mut sched, &mut scratch, &song, n, &rec, ModTickPlaneRef::default(), io);
        (scratch[0].track_l[..n].to_vec(), scratch[0].track_r[..n].to_vec())
    };
    let chain = vec![Device::Native(hard_comp(10)), Device::Native(eq_boost(11))];
    let listened = render(chain.clone(), NativeIo { sc_listen: 10, scopes: None });
    assert_eq!(listened, render(vec![], NativeIo::default()));
    assert_ne!(render(chain, NativeIo::default()), listened);
}

/// T12: 内蔵 device の op (Staged / OwnPreFx / crossfade / Listen / scope)、SC の staging、PreFx tap を持つ
/// track の `process_track_owned` は RT で確保も解放もしない (`cargo test -p daw_audio --features rt-assert`)。
#[cfg(feature = "rt-assert")]
#[test]
fn native_ops_and_sidechain_staging_do_not_allocate() {
    use common::device_scope_bridge::{DeviceScopeBridgeHandle, MAX_DEVICE_SCOPES};
    use common::protocol::ProjectKey;

    let n = 256;
    let song = Song {
        tracks: vec![
            track(|t| t.id = 1),
            track(|t| {
                t.id = 2;
                t.devices = vec![
                    Device::Native(with_sc(hard_comp(20), TapSource::Track(1), TapPoint::PostFader)),
                    Device::Native(with_sc(hard_comp(21), TapSource::Track(2), TapPoint::PreFx)),
                    Device::Native(hard_comp(22)),
                    Device::Native(eq_boost(23)),
                ];
            }),
        ],
        ..Song::default()
    };
    let bypassed = {
        let mut s = song.clone();
        s.tracks[1].devices[2] = Device::Native(NativeDevice { bypassed: true, ..hard_comp(22) });
        s
    };
    let mut sched = compile_schedule_for_test(&song, SR, n as u32).expect("compile");
    let taps = native_taps(&sched);
    let mut scratch = scratches(2);
    let x = sine(n, 0, 440.0, 0.7);
    let bridge = DeviceScopeBridgeHandle::create(&format!("daw01_test_native_rt_{}", std::process::id())).unwrap();
    let mut watch = [0u64; MAX_DEVICE_SCOPES];
    watch[3] = 23;
    bridge.set_slot(3, ProjectKey(1), 23);
    let io = NativeIo { sc_listen: 21, scopes: Some(DeviceScopeTap { bridge: &bridge, watch: &watch }) };
    let refs: PluginRefs = HashMap::new();
    let rec = HashSet::new();
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut midi_a, mut midi_b) = (Vec::with_capacity(MAX_EVENTS), Vec::with_capacity(MAX_EVENTS));
    let mut step = |b: usize, song: &Song, scratch: &mut Vec<TrackScratch>, sched: &mut Schedule| {
        scratch[0].track_l[..n].copy_from_slice(&x);
        scratch[0].track_r[..n].copy_from_slice(&x);
        for (src, owner, slot) in &taps {
            stage_native_sidechain(scratch, &mut sched.track_programs, &mut sched.master_program, *src, *owner, *slot, n);
        }
        l.copy_from_slice(&x);
        r.copy_from_slice(&x);
        let devices = &song.tracks[1].devices;
        let ctx = ProgramCtx {
            song: Some(song),
            plugin_refs: &refs,
            worker_sync: None,
            sample_rate: SR,
            frames: n as u32,
            playing: true,
            current_bpm: 120.0,
            playhead_beats: 0.0,
            loop_region: LoopRegion::default(),
            recording_lanes: &rec,
            mod_plane: ModTickPlaneRef::default(),
            rows: TrackRows::default(),
            own_pre_fx: Some((&x[..], &x[..])),
            native: io,
            owner_devices: devices,
            owner_stores: (&[], &[]),
        };
        let p = &mut sched.track_programs[1];
        let len = p.ops.len();
        run_chain_program(p, 0..len, &mut l, &mut r, &mut midi_a, &mut midi_b, &ctx);
        apply_listen_override(p, &mut l, &mut r, n);
        // PreFx tap (自トラック Pre-FX を読む Comp 21) を持つ track の pass 1 全体。
        crate::graph::process_track_owned(
            1, &song.tracks[1], &mut scratch[1], p, &refs, None, None, SR, n as u32, true, Some(song), false, 0,
            &rec, 120.0, b as f64, LoopRegion::default(), ModTickPlaneRef::default(), TrackRows::default(), io,
        );
    };
    step(0, &song, &mut scratch, &mut sched); // warm-up
    assert_no_alloc::assert_no_alloc(|| {
        for b in 1..12 {
            // 途中で bypass を切り替えて crossfade も通す。
            let s = if (4..8).contains(&b) { &bypassed } else { &song };
            step(b, s, &mut scratch, &mut sched);
        }
    });
}
