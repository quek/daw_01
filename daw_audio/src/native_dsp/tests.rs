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

/// T8 (DSP 側): 遅延を焼いてある間、解決値が OFF なら先読みぶんの遅延だけを通してゲインは掛けない
/// (PDC の会計どおり)。遅延を焼いていなければ遅延もゲインも無い素通し。`reset()` 後のリングは無音。
#[test]
fn master_limiter_passes_only_the_delay_while_resolved_off_and_resets_to_silence() {
    let look = common::model::limiter_lookahead_samples(SR) as usize;
    let n = 512;
    let loud = vec![2.0f32; n];
    let off = MasterLimiterSettings { on: false, ceiling_db: -1.0 };
    let mut st = MasterLimiterState::new();
    let (mut l, mut r) = (loud.clone(), loud.clone());
    st.process(&off, true, &mut l, &mut r, n, SR as f32);
    assert!(l[..look].iter().all(|v| *v == 0.0), "先読みぶんの無音が先行する");
    assert!(l[look..].iter().chain(&r[look..]).all(|v| *v == 2.0), "ゲインは掛けない (+6 dB のまま)");
    assert_eq!(st.gain_reduction_db(), 0.0);

    let mut bare = MasterLimiterState::new();
    let (mut l, mut r) = (loud.clone(), loud.clone());
    bare.process(&off, false, &mut l, &mut r, n, SR as f32);
    assert_eq!((l, r), (loud.clone(), loud), "遅延を焼いていなければ素通し");

    let on = MasterLimiterSettings { on: true, ceiling_db: -1.0 };
    let (mut l, mut r) = (vec![0.9f32; n], vec![0.9f32; n]);
    st.process(&on, true, &mut l, &mut r, n, SR as f32);
    st.reset();
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    st.process(&on, true, &mut l, &mut r, n, SR as f32);
    assert!(l.iter().chain(&r).all(|v| *v == 0.0), "reset 後のリングに前の音が残らない");
    assert_eq!(st.gain_reduction_db(), 0.0);
}

/// Limiter はどんな刺激・入力レベル・ceiling・ブロック長でも出力が ceiling を超えない
/// (旧実装は先読み窓の中でピークを保持せず、孤立インパルスで超えていた)。ceiling 以下の音は先読みぶん
/// 遅れるだけでビット単位で変わらない。
#[test]
fn master_limiter_never_exceeds_the_ceiling() {
    let look = common::model::limiter_lookahead_samples(SR) as usize;
    let mut worst_ratio = 0.0f64;
    for stimulus in Stimulus::ALL {
        for input_gain_db in [0.0, 6.0, 12.0, 24.0] {
            let x = stimulus.generate(input_gain_db);
            for ceiling_db in [-6.0f32, -1.0, 0.0] {
                let ceiling_amp = common::dsp::db_to_amp(ceiling_db);
                let s = MasterLimiterSettings { on: true, ceiling_db };
                for block in [64usize, 512, 1024] {
                    let mut st = MasterLimiterState::new();
                    let (mut l, mut r) = (x.l.clone(), x.r.clone());
                    for (cl, cr) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
                        let n = cl.len();
                        st.process(&s, true, cl, cr, n, SR as f32);
                        assert!(st.gain_reduction_db() <= 0.0);
                    }
                    let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
                    worst_ratio = worst_ratio.max(f64::from(peak) / f64::from(ceiling_amp));
                    assert!(
                        peak <= ceiling_amp,
                        "{} +{input_gain_db}dB ceiling {ceiling_db} block {block}: peak {peak} > {ceiling_amp}",
                        stimulus.name()
                    );
                }
            }
        }
    }
    assert!(worst_ratio > 0.99, "どこかで ceiling まで潰している (検査が空振りしていない): {worst_ratio}");

    // ceiling 以下の音は遅延だけ。
    let quiet = Stimulus::Tones.generate(-12.0);
    let mut st = MasterLimiterState::new();
    let (mut l, mut r) = (quiet.l.clone(), quiet.r.clone());
    for (cl, cr) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
        let n = cl.len();
        st.process(&MasterLimiterSettings { on: true, ceiling_db: 0.0 }, true, cl, cr, n, SR as f32);
        assert_eq!(st.gain_reduction_db(), 0.0);
    }
    assert_eq!(&l[look..], &quiet.l[..quiet.l.len() - look]);
    assert_eq!(&r[look..], &quiet.r[..quiet.r.len() - look]);
}

/// §12.2: 上流が非有限のサンプル (±inf / NaN) を 1 サンプル出しても、Limiter の状態は固着しない。
/// 以後の出力は全サンプル有限で ceiling を超えず、ceiling 以下の音に戻れば遅延だけの素通しに戻る
/// (live と書き出しは同じ `process` を通るので、これ 1 本で両方を覆う)。
#[test]
fn master_limiter_contains_a_non_finite_sample() {
    let look = common::model::limiter_lookahead_samples(SR) as usize;
    let block = 256;
    let s = MasterLimiterSettings { on: true, ceiling_db: -1.0 };
    let ceiling_amp = common::dsp::db_to_amp(s.ceiling_db);
    for poison in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
        for (poison_l, poison_r) in [(true, false), (false, true), (true, true)] {
            let label = format!("{poison} L={poison_l} R={poison_r}");
            let mut st = MasterLimiterState::new();
            // 潰している最中 (+6 dB) に 1 サンプルだけ非有限。
            for b in 0..20 {
                let (mut l, mut r) = (vec![2.0f32; block], vec![-2.0f32; block]);
                if b == 3 {
                    if poison_l {
                        l[17] = poison;
                    }
                    if poison_r {
                        r[17] = poison;
                    }
                }
                st.process(&s, true, &mut l, &mut r, block, SR as f32);
                assert!(
                    l.iter().chain(&r).all(|v| v.is_finite() && v.abs() <= ceiling_amp),
                    "{label} block {b}: 出力が非有限か ceiling 超え"
                );
                assert!(st.gain_reduction_db().is_finite() && st.gain_reduction_db() <= 0.0, "{label} block {b}");
            }
            // ceiling 以下に戻ってリリースし終えたら、遅延だけの素通し (無音にも NaN にも固着しない)。
            let quiet: Vec<f32> = (0..block * 400).map(|i| 0.25 * ((i as f32) * 0.01).sin()).collect();
            let (mut l, mut r) = (quiet.clone(), quiet.clone());
            for (cl, cr) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
                st.process(&s, true, cl, cr, block, SR as f32);
            }
            assert_eq!(st.gain_reduction_db(), 0.0, "{label}: リリースし終えている");
            let tail = quiet.len() - block * 8;
            for i in tail..quiet.len() {
                assert!((l[i] - quiet[i - look]).abs() <= 1e-6, "{label} sample {i}: {} != {}", l[i], quiet[i - look]);
                assert!((r[i] - quiet[i - look]).abs() <= 1e-6, "{label} sample {i}");
            }
        }
    }
}

/// T12: Limiter (先読みリング / 窓内最小値の deque / 移動平均) は RT で確保しない。SR の変更・OFF の区間・
/// reset も含めて回す。
#[cfg(feature = "rt-assert")]
#[test]
fn master_limiter_does_not_allocate() {
    let x = Stimulus::Impulses.generate(12.0);
    let mut st = MasterLimiterState::new();
    let (mut l, mut r) = (x.l.clone(), x.r.clone());
    assert_no_alloc::assert_no_alloc(|| {
        for (i, (cl, cr)) in l.chunks_mut(256).zip(r.chunks_mut(256)).enumerate() {
            let n = cl.len();
            let s = MasterLimiterSettings { on: i % 7 != 3, ceiling_db: -3.0 };
            let sr = if i < 200 { 48_000.0 } else { 96_000.0 };
            st.process(&s, i % 11 != 5, cl, cr, n, sr);
            if i == 300 {
                st.reset();
            }
        }
    });
}

/// golden の `limiter.on = true` のシナリオ (master_full) の窓を、直した Limiter で取り直す。
/// それ以外のシナリオと meta は変えない。ヘッダの記録条件の行も合わせて書き換える (冪等)。
#[test]
#[ignore = "r.md #129 E: Limiter の修正後に golden の master_full を取り直す"]
fn rerecord_limiter_scenarios() {
    let path = dsp_golden::golden_path();
    let text = std::fs::read_to_string(&path).expect("golden_v38.txt を読めない");
    let mut golden = dsp_golden::parse(&text).expect("golden を解釈できない");
    let mut rerecorded = 0usize;
    for s in &mut golden.scenarios {
        if s.meta("limiter.on") == Some("true") {
            s.windows = render(s);
            rerecorded += 1;
        }
    }
    assert!(rerecorded > 0);
    for h in &mut golden.header {
        if h.starts_with("記録:") {
            *h = "記録: 旧 DSP の記録器 (旧 mixer/channel_strip.rs の tests) は旧型と一緒に削除済み。limiter.on=true の窓の取り直しは cargo test -p daw_audio --bin daw_audio -- --ignored rerecord_limiter_scenarios (native_dsp/tests.rs)。".to_string();
        } else if h.starts_with("kind=master_full:") {
            *h = "kind=master_full: ブロックごとに process_pre → process_limiter (HEAD の render_master_buffer と同じ順。master_fx_chain は空・master_gain は 1.0 なので間に処理は無い)。limiter.on=true = HEAD でリミッターの先読み遅延が乗る状態。gr = gain_reduction_db().0、lim_gr = .1。**この kind の窓だけは r.md #129 E で直した Limiter (native_dsp/limiter.rs、先読み窓の中で必要ゲインの最小値を保持) から取り直した** — 旧 Limiter は先読みしている間にリリースで利得が戻り、孤立インパルスで ceiling を超えていた (peak 1.003 > −1 dBFS)。Bus Comp / Tone EQ の部分は旧 DSP と同じ。".to_string();
        }
    }
    let out = dsp_golden::format(&golden);
    assert_eq!(dsp_golden::parse(&out).as_ref(), Ok(&golden), "書いた値を読み戻せない");
    std::fs::write(&path, out).expect("golden を書けない");
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
