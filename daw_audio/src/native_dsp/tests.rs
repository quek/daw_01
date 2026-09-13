//! r.md #129 §15.3 T1: 移植した内蔵 DSP が旧 strip DSP の golden (`golden_v38.txt`) と一致する。
//!
//! 刺激・窓統計・形式は `crate::dsp_golden` (記録側と同じコード)。ここは比較側で、各シナリオの meta
//! から内蔵 device と Limiter の設定を組み、**本番と同じ経路** (`build_program` → `run_chain_program`
//! → `MasterLimiterState`) で走らせて窓ごとに比べる。許容誤差は `|Δ| ≤ 1e-6 + 1e-5·|ref|`、
//! `|Δgr| ≤ 1e-4 dB` (§8.8 の CPU 最適化で出る丸めの差を含む)。

use std::collections::{HashMap, HashSet};

use common::model::{
    BusCompParam, CompMode, CompSettings, Device, EqBand, EqSettings, MASTER_TRACK_ID, MasterLimiterSettings,
    NativeDevice, NativeKind, NativeParamId, NativeParams, Song, ToneEqSettings,
};

use crate::dsp_golden::{self, BlockGr, Scenario, Stimulus, WindowStats};
use crate::engine::PluginRefs;
use crate::graph::{DeviceLatencies, NativeIo, ProgramCtx, build_program, run_chain_program};
use crate::launcher::TrackRows;
use crate::mixer::MAX_EVENTS;
use crate::native_dsp::MasterLimiterState;
use common::mod_plane::ModTickPlaneRef;

const SR: u32 = dsp_golden::SAMPLE_RATE;

fn meta<'a>(s: &'a Scenario, key: &str) -> &'a str {
    s.meta(key).unwrap_or_else(|| panic!("{}: meta {key} が無い", s.name))
}

fn meta_f32(s: &Scenario, key: &str) -> f32 {
    meta(s, key).parse().unwrap_or_else(|e| panic!("{}: meta {key}: {e}", s.name))
}

fn meta_bool(s: &Scenario, key: &str) -> bool {
    meta(s, key).parse().unwrap_or_else(|e| panic!("{}: meta {key}: {e}", s.name))
}

/// strip 由来 (`strip_comp` / `strip_eq` / `strip_full`) の `[Comp, EQ]`。旧 strip の `on` は
/// `bypassed = !on`。
fn strip_devices(s: &Scenario) -> Vec<Device> {
    let mut comp = NativeDevice::new_builtin(NativeKind::Comp, 1);
    comp.bypassed = !meta_bool(s, "comp.on");
    if !comp.bypassed {
        let mode = match meta(s, "comp.mode") {
            "Leveler" => CompMode::Leveler,
            "Compressor" => CompMode::Compressor,
            "Limiter" => CompMode::Limiter,
            other => panic!("{}: comp.mode {other}", s.name),
        };
        comp.params = NativeParams::Comp(CompSettings {
            mode,
            threshold_db: meta_f32(s, "comp.threshold_db"),
            ratio: meta_f32(s, "comp.ratio"),
            attack_ms: meta_f32(s, "comp.attack_ms"),
            release_ms: meta_f32(s, "comp.release_ms"),
            makeup_db: meta_f32(s, "comp.makeup_db"),
            sc_freq_hz: meta_f32(s, "comp.sc_freq_hz"),
        });
    }
    let mut eq = NativeDevice::new_builtin(NativeKind::Eq, 2);
    eq.bypassed = !meta_bool(s, "eq.on");
    if !eq.bypassed {
        let mut e = EqSettings::default();
        for band in EqBand::ALL {
            let p = format!("eq.{}", band.label().to_lowercase());
            let b = e.band_mut(band);
            b.on = meta_bool(s, &format!("{p}.on"));
            b.freq_hz = meta_f32(s, &format!("{p}.freq_hz"));
            b.gain_db = meta_f32(s, &format!("{p}.gain_db"));
            b.q = meta_f32(s, &format!("{p}.q"));
            b.bell = meta_bool(s, &format!("{p}.bell"));
        }
        eq.params = NativeParams::Eq(e);
    }
    vec![Device::Native(comp), Device::Native(eq)]
}

/// master 由来 (`bus_comp` / `tone_eq` / `master_full`) の `[Bus Comp, Tone EQ]` と Limiter。
/// 段階式は `*_index` (旧 `MasterStripParam` の plain 値 = 新 Stepped の plain 値) を住所経由で書く。
fn master_devices(s: &Scenario) -> (Vec<Device>, MasterLimiterSettings) {
    let mut bus = NativeDevice::new_builtin(NativeKind::BusComp, 1);
    bus.bypassed = !meta_bool(s, "comp.on");
    if !bus.bypassed {
        for (p, key) in [
            (BusCompParam::Threshold, "comp.threshold_db"),
            (BusCompParam::Ratio, "comp.ratio_index"),
            (BusCompParam::Attack, "comp.attack_index"),
            (BusCompParam::Release, "comp.release_index"),
            (BusCompParam::Makeup, "comp.makeup_db"),
        ] {
            let v = meta_f32(s, key);
            bus.set_param(NativeParamId::BusComp(p), v);
            assert_eq!(bus.param(NativeParamId::BusComp(p)), Some(v), "{}: {key} が値域外", s.name);
        }
    }
    let mut tone = NativeDevice::new_builtin(NativeKind::ToneEq, 2);
    tone.bypassed = !meta_bool(s, "eq.on");
    if !tone.bypassed {
        tone.params = NativeParams::ToneEq(ToneEqSettings {
            low_db: meta_f32(s, "eq.low_db"),
            lomid_db: meta_f32(s, "eq.lomid_db"),
            high_db: meta_f32(s, "eq.high_db"),
        });
    }
    let on = meta_bool(s, "limiter.on");
    let limiter = MasterLimiterSettings {
        on,
        ceiling_db: if on { meta_f32(s, "limiter.ceiling_db") } else { MasterLimiterSettings::default().ceiling_db },
    };
    (vec![Device::Native(bus), Device::Native(tone)], limiter)
}

/// シナリオを本番の経路で走らせて窓統計を返す。
fn render(s: &Scenario) -> Vec<WindowStats> {
    let kind = meta(s, "kind");
    let stimulus = Stimulus::from_name(meta(s, "stimulus")).expect("stimulus");
    let block: usize = meta(s, "block").parse().expect("block");
    let input_gain_db: f64 = meta(s, "input_gain_db").parse().expect("input_gain_db");
    let master = match kind {
        "strip_comp" | "strip_eq" | "strip_full" => false,
        "bus_comp" | "tone_eq" | "master_full" => true,
        other => panic!("{}: 未知の kind {other}", s.name),
    };
    let (devices, limiter) =
        if master { master_devices(s) } else { (strip_devices(s), MasterLimiterSettings::default()) };
    // 遅延を焼くか = `Song::master_limiter_latency_active` (レーンも変調も無いので静的な on)。
    let mut song = Song { master_limiter: limiter, ..Song::default() };
    let latency_active = song.master_limiter_latency_active();
    song.master_fx_chain = Vec::new();
    let track_id = if master { MASTER_TRACK_ID } else { 1 };
    let mut program = build_program(&devices, track_id, None, &DeviceLatencies::new(), &HashSet::new()).program;
    let mut limiter_state = MasterLimiterState::new();
    let refs: PluginRefs = HashMap::new();
    let recording = HashSet::new();
    let (mut midi_a, mut midi_b) = (Vec::with_capacity(MAX_EVENTS), Vec::with_capacity(MAX_EVENTS));
    dsp_golden::run_blocks(&stimulus.generate(input_gain_db), block, |l, r| {
        let n = l.len();
        let ctx = ProgramCtx {
            song: Some(&song),
            plugin_refs: &refs,
            worker_sync: None,
            sample_rate: SR,
            frames: n as u32,
            playing: true,
            current_bpm: 120.0,
            playhead_beats: 0.0,
            loop_region: common::model::LoopRegion::default(),
            recording_lanes: &recording,
            mod_plane: ModTickPlaneRef::default(),
            rows: TrackRows::default(),
            own_pre_fx: None,
            native: NativeIo::default(),
            owner_devices: &devices,
            owner_stores: (&[], &[]),
        };
        let len = program.ops.len();
        run_chain_program(&mut program, 0..len, l, r, &mut midi_a, &mut midi_b, &ctx);
        limiter_state.process(&song.master_limiter, latency_active, l, r, n, SR as f32);
        BlockGr { gr: program.natives[0].gr_db, lim_gr: limiter_state.gain_reduction_db() }
    })
}

fn within(got: f64, want: f64) -> bool {
    (got - want).abs() <= 1e-6 + 1e-5 * want.abs()
}

/// T1: golden の全シナリオ × 全窓 × 全統計が許容誤差に収まる。
#[test]
fn native_dsp_matches_the_v38_strip_golden_in_every_scenario() {
    let text = std::fs::read_to_string(dsp_golden::golden_path()).expect("golden_v38.txt を読めない");
    let golden = dsp_golden::parse(&text).expect("golden を解釈できない");
    assert!(!golden.scenarios.is_empty());
    let mut failures: Vec<String> = Vec::new();
    let mut total = 0usize;
    for s in &golden.scenarios {
        let got = render(s);
        assert_eq!(got.len(), s.windows.len(), "{}: 窓の数", s.name);
        for (w, (g, e)) in got.iter().zip(&s.windows).enumerate() {
            let stats = [
                ("rms_l", g.rms_l, e.rms_l),
                ("rms_r", g.rms_r, e.rms_r),
                ("peak_l", g.peak_l, e.peak_l),
                ("peak_r", g.peak_r, e.peak_r),
                ("sig_l", g.sig_l, e.sig_l),
                ("sig_r", g.sig_r, e.sig_r),
            ];
            for (name, got, want) in stats {
                if !within(got, want) {
                    total += 1;
                    if failures.len() < 40 {
                        failures.push(format!("{} w{w} {name}: got {got:?} want {want:?}", s.name));
                    }
                }
            }
            for (name, got, want) in [("gr", g.gr, e.gr), ("lim_gr", g.lim_gr, e.lim_gr)] {
                if (got - want).abs() > 1e-4 {
                    total += 1;
                    if failures.len() < 40 {
                        failures.push(format!("{} w{w} {name}: got {got:?} want {want:?}", s.name));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "golden と {total} 件食い違う:\n{}", failures.join("\n"));
}
