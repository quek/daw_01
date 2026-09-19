//! `ChainProgram` の compile 側 (off-RT): device ツリー → 命令列 + scratch + 並列 PDC。
//! `docs/plan_parallel.md` §4.1 / §4.2。

use std::collections::{HashMap, HashSet};

use common::model::{Device, TapPoint, TapSource};
use common::protocol::RenderScope;

use super::compile::DeviceLatencies;
use super::delay_line::DelayLine;
use super::native::NativeScratch;
use super::program::{ChainOp, ChainProgram, ChainScratch, ParallelScratch};
use crate::mixer::MAX_FRAMES;

/// chain の tap 点ごとの **track chain 起点からの相対 latency** (samples)。
/// sidechain / follower の source latency 計算に使う。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainLatency {
    /// `PreFx` = Parallel 入力 (Parallel の手前までの累積)。
    pub parallel_input: u32,
    /// `PostFx` = chain の device 通過後 (PDC delay の前)。
    pub post_fx: u32,
    /// `PostFader` = 並列 PDC で Parallel 内最大に揃えた後。
    pub post_fader: u32,
}

/// [`build_program`] の結果。
pub struct BuiltProgram {
    pub program: ChainProgram,
    /// この device 列全体の latency (= 旧 `chain_latency`)。
    pub latency: u32,
    /// chain id → tap 点別 latency。
    pub chain_latency: HashMap<u64, ChainLatency>,
    /// chain id → slot 情報 (tap の `BufRef` 解決用)。
    pub chain_slots: HashMap<u64, ChainSlot>,
    /// r.md #129: 内蔵 device id → `ChainProgram::natives` の slot。op が出た device だけが載る
    /// (bypass 中の Parallel の中の native には op も slot も無い) ので、SC の tap を出せるかの判定にも使う。
    pub native_slots: HashMap<u64, u32>,
}

/// chain の program 内の位置。 `output` = r.md #112 `Split` でこの chain が受ける出力番号
/// (`PreFx` tap はその出力を指す)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainSlot {
    pub chain_slot: u32,
    pub parallel_slot: u32,
    pub output: Option<u8>,
}

/// plugin の op を出すか: bypass 中は出さない。`scope` がトラックの fx を通さない (`RenderScope::Sources`) なら、
/// 音声入力を持つ device (= 入ってくる音を加工する) も出さない — 音源 (音声入力を持たない device) は残す。
/// op と latency の会計はどちらもこれを引く (出さない device の latency を PDC に数えない)。
/// r.md #131: 読み込み中の plugin がトラックを待たせるかもこれを引く (`compile::executable_tracks`)。
pub(crate) fn plugin_in_scope(p: &common::model::PluginInstance, scope: RenderScope) -> bool {
    !p.bypassed && (scope.track_fx() || !p.ports.has_audio_input)
}

/// `devices` (bypass 中と `scope` の外は除く) の直列 latency。Parallel は chain の最大。
pub fn program_latency(devices: &[Device], latencies: &DeviceLatencies, scope: RenderScope) -> u32 {
    devices.iter().fold(0u32, |acc, d| match d {
        Device::Plugin(p) => {
            if plugin_in_scope(p, scope) {
                acc.saturating_add(latencies.get(&p.id).copied().unwrap_or(0))
            } else {
                acc
            }
        }
        // r.md #129: 内蔵 device の遅延は 0。
        Device::Native(_) => acc,
        Device::Parallel(r) => {
            if r.bypassed {
                acc
            } else {
                acc.saturating_add(
                    r.chains
                        .iter()
                        .map(|c| program_latency(&c.devices, latencies, scope))
                        .max()
                        .unwrap_or(0),
                )
            }
        }
    })
}

/// `devices` を展開する。`split_top` = パラアウトの top-level split index
/// (`Track::paraout_split_device`、`None` = 全部 pass 1)。`taps` = 誰かが読む
/// `(chain_id, tap_point)` の集合 (snapshot flag に焼く)。`scope` = どの処理段を通すか
/// (通さない device の op を出さない / Parallel の混ぜ方 / フェーダーを掛けるか を program に焼く)。
pub fn build_program(
    devices: &[Device],
    track_id: u32,
    split_top: Option<u32>,
    latencies: &DeviceLatencies,
    taps: &HashSet<(u64, TapPoint)>,
    scope: RenderScope,
    sample_rate: u32,
) -> BuiltProgram {
    let mut program = ChainProgram::empty(track_id);
    program.fader = scope.fader();
    let mut chain_latency: HashMap<u64, ChainLatency> = HashMap::new();
    let mut chain_slots: HashMap<u64, ChainSlot> = HashMap::new();
    let mut native_slots: HashMap<u64, u32> = HashMap::new();
    let mut b = Builder {
        program: &mut program,
        latencies,
        taps,
        scope,
        chain_latency: &mut chain_latency,
        chain_slots: &mut chain_slots,
        native_slots: &mut native_slots,
        sample_rate,
    };
    let mut acc = 0u32;
    let mut pass1_end: Option<usize> = None;
    for (i, d) in devices.iter().enumerate() {
        acc = acc.saturating_add(b.emit_device(d, acc));
        if split_top == Some(i as u32 + 1) {
            pass1_end = Some(b.program.ops.len());
        }
    }
    let len = program.ops.len();
    program.pass1_end = pass1_end.unwrap_or(len).min(len);
    // crossfade の dry 退避は内蔵 device が 1 つでもあるときだけ (op は直列なので 1 組)。
    if !program.natives.is_empty() {
        program.native_dry_l = vec![0.0; MAX_FRAMES];
        program.native_dry_r = vec![0.0; MAX_FRAMES];
    }
    program.index_state_keys();
    BuiltProgram {
        program,
        latency: acc,
        chain_latency,
        chain_slots,
        native_slots,
    }
}

struct Builder<'a> {
    program: &'a mut ChainProgram,
    latencies: &'a DeviceLatencies,
    taps: &'a HashSet<(u64, TapPoint)>,
    scope: RenderScope,
    chain_latency: &'a mut HashMap<u64, ChainLatency>,
    chain_slots: &'a mut HashMap<u64, ChainSlot>,
    native_slots: &'a mut HashMap<u64, u32>,
    /// セッションのサンプルレート (Reverb / Delay の遅延メモリの確保に要る)。
    sample_rate: u32,
}

impl Builder<'_> {
    /// `devices` を順に emit し、この列の latency を返す。`prefix` = 列の手前までの累積。
    fn emit_list(&mut self, devices: &[Device], prefix: u32) -> u32 {
        let mut acc = 0u32;
        for d in devices {
            acc = acc.saturating_add(self.emit_device(d, prefix.saturating_add(acc)));
        }
        acc
    }

    /// device 1 つを emit し、その latency を返す。
    fn emit_device(&mut self, d: &Device, prefix: u32) -> u32 {
        match d {
            Device::Plugin(p) => {
                if !plugin_in_scope(p, self.scope) {
                    return 0;
                }
                let voice_slot = self.program.voices.len() as u32;
                self.program.voices.push(crate::graph::voices::VoiceTable::new(p.id));
                self.program.ops.push(ChainOp::Plugin {
                    device_id: p.id,
                    ports: p.ports,
                    own_prefx_ports: own_prefx_ports(p, self.program.track_id),
                    voice_slot,
                });
                self.latencies.get(&p.id).copied().unwrap_or(0)
            }
            // r.md #129: 内蔵 device。bypass 中でも op を出す (実効 ON/OFF は RT が block 頭で解決し、
            // 切り替えは crossfade)。未採番 (id 0) は引き当てられないので出さない。遅延は 0。
            // 内蔵 device はどれも入ってくる音を加工するので、トラックの fx を通さない scope では出さない。
            Device::Native(nd) => {
                if nd.id == 0 || !self.scope.track_fx() {
                    return 0;
                }
                let native_slot = self.program.natives.len() as u32;
                self.program.natives.push(NativeScratch::new(nd, self.program.track_id, self.sample_rate));
                self.program.ops.push(ChainOp::Native { device_id: nd.id, native_slot });
                self.native_slots.insert(nd.id, native_slot);
                0
            }
            Device::Parallel(r) => {
                // chain を全部消した Parallel は Live / Bitwig と同じく素通し (op を出さない =
                // 何も無いのと同じ)。 bypass も同じ。
                if r.bypassed || r.chains.is_empty() {
                    return 0;
                }
                self.emit_parallel(r, prefix)
            }
        }
    }

    /// Parallel 1 つを emit し、その latency (= chain の最大) を返す。
    ///
    /// トラックの fx を通さない scope (`RenderScope::Sources`) では「素材の音だけ」を描く Parallel にする
    /// (`program` の module doc)。帯域分割 / Mid-Side は入力の加工なので置かない。Selector は MIDI を
    /// アクティブ chain だけへ配る (= どの音源が鳴るか) ので残す。
    fn emit_parallel(&mut self, r: &common::model::Parallel, prefix: u32) -> u32 {
        let sources = !self.scope.track_fx();
        let split = match r.split {
            common::model::Split::Selector { .. } => r.split,
            _ if sources => common::model::Split::None,
            _ => r.split,
        };
        let parallel_slot = self.program.parallels.len() as u32;
        self.program.parallels.push(ParallelScratch::new(r.id, split, r.chains.len(), sources));
        self.program.ops.push(ChainOp::ParallelBegin { parallel_slot });
        let max = r
            .chains
            .iter()
            .map(|c| program_latency(&c.devices, self.latencies, self.scope))
            .max()
            .unwrap_or(0);
        for (k, c) in r.chains.iter().enumerate() {
            let chain_slot = self.program.chains.len() as u32;
            let output = split.output_of(k);
            self.program.chains.push(ChainScratch::new(c.id));
            self.chain_slots.insert(c.id, ChainSlot { chain_slot, parallel_slot, output });
            self.program.ops.push(ChainOp::ChainBegin { parallel_slot, chain_slot, output });
            let lat = self.emit_list(&c.devices, prefix);
            let delay = (max > lat).then(|| {
                let comp = max - lat;
                let line_idx = self.program.delay_lines.len() as u32;
                self.program.delay_lines.push(DelayLine::with_capacity(comp as usize + 1));
                self.program.delay_keys.push(c.id);
                (line_idx, comp)
            });
            self.chain_latency.insert(
                c.id,
                ChainLatency {
                    parallel_input: prefix,
                    post_fx: prefix.saturating_add(lat),
                    post_fader: prefix.saturating_add(max),
                },
            );
            self.program.ops.push(ChainOp::ChainEnd {
                parallel_slot,
                chain_slot,
                parallel_id: r.id,
                chain_id: c.id,
                delay,
                snapshot_post_fx: self.taps.contains(&(c.id, TapPoint::PostFx)),
                snapshot_post_fader: self.taps.contains(&(c.id, TapPoint::PostFader)),
            });
        }
        // 素材の音だけ: chain を通らない入力も chain の最大 latency に揃える (key は Parallel id)。
        let input_delay = (sources && max > 0).then(|| {
            let line_idx = self.program.delay_lines.len() as u32;
            self.program.delay_lines.push(DelayLine::with_capacity(max as usize + 1));
            self.program.delay_keys.push(r.id);
            (line_idx, max)
        });
        self.program.ops.push(ChainOp::ParallelEnd { parallel_slot, parallel_id: r.id, input_delay });
        max
    }
}

/// `ChainOp::Plugin::own_prefx_ports`: 自 track の Pre-FX を source にする aux port の bit 集合
/// (`SidechainTap` では運ばない = compile 側の `emit_aux_input_taps` が同じ条件で除外する)。
pub(crate) fn own_prefx_ports(p: &common::model::PluginInstance, track_id: u32) -> u8 {
    p.aux_inputs
        .iter()
        .take(common::process_data::MAX_AUX_IN.min(8))
        .enumerate()
        .filter(|(_, r)| {
            r.is_some_and(|r| r.tap.source == TapSource::Track(track_id) && r.tap.tap_point == TapPoint::PreFx)
        })
        .fold(0u8, |acc, (k, _)| acc | (1 << k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{PluginInstance, Parallel, ParallelChain};
    use common::plugin_format::PluginFormat;

    fn plug(id: u64) -> Device {
        Device::Plugin(PluginInstance {
            id,
            ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap)
        })
    }

    fn parallel(id: u64, chains: Vec<(u64, Vec<Device>)>) -> Device {
        Device::Parallel(Parallel {
            id,
            name: "Parallel".into(),
            chains: chains
                .into_iter()
                .map(|(cid, devices)| ParallelChain {
                    id: cid,
                    devices,
                    ..ParallelChain::new("c")
                })
                .collect(),
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
            split: common::model::Split::None,
        })
    }

    fn op_kinds(p: &ChainProgram) -> Vec<String> {
        p.ops
            .iter()
            .map(|op| match op {
                ChainOp::Plugin { device_id, .. } => format!("P{device_id}"),
                ChainOp::ParallelBegin { parallel_slot } => format!("RB{parallel_slot}"),
                ChainOp::ChainBegin { chain_slot, .. } => format!("CB{chain_slot}"),
                ChainOp::ChainEnd { chain_slot, delay, .. } => match delay {
                    Some((_, f)) => format!("CE{chain_slot}d{f}"),
                    None => format!("CE{chain_slot}"),
                },
                ChainOp::ParallelEnd { parallel_slot, .. } => format!("RE{parallel_slot}"),
                ChainOp::Native { device_id, .. } => format!("N{device_id}"),
            })
            .collect()
    }

    #[test]
    fn nested_parallel_expands_to_flat_ops_in_signal_order() {
        let devices = vec![
            plug(1),
            parallel(10, vec![(11, vec![plug(2), parallel(20, vec![(21, vec![plug(3)])])]), (12, vec![])]),
            plug(4),
        ];
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &HashSet::new(), RenderScope::Mix, 48_000);
        assert_eq!(
            op_kinds(&b.program),
            vec![
                "P1", "RB0", "CB0", "P2", "RB1", "CB1", "P3", "CE1", "RE1", "CE0", "CB2", "CE2",
                "RE0", "P4"
            ]
        );
        assert_eq!(b.program.pass1_end, b.program.ops.len());
        assert_eq!(b.program.parallels.len(), 2);
        assert_eq!(b.program.chains.len(), 3);
    }

    #[test]
    fn parallel_pdc_delays_the_shorter_chain_and_reports_max() {
        let mut lat = DeviceLatencies::new();
        lat.insert(2, 100);
        lat.insert(3, 40);
        lat.insert(1, 5);
        let devices = vec![plug(1), parallel(10, vec![(11, vec![plug(2)]), (12, vec![plug(3)]), (13, vec![])])];
        let b = build_program(&devices, 7, None, &lat, &HashSet::new(), RenderScope::Mix, 48_000);
        assert_eq!(b.latency, 105, "5 + max(100, 40, 0)");
        assert_eq!(
            op_kinds(&b.program),
            vec!["P1", "RB0", "CB0", "P2", "CE0", "CB1", "P3", "CE1d60", "CB2", "CE2d100", "RE0"]
        );
        assert_eq!(b.program.delay_keys, vec![12, 13]);
        assert_eq!(
            b.chain_latency[&12],
            ChainLatency { parallel_input: 5, post_fx: 45, post_fader: 105 }
        );
    }

    #[test]
    fn bypassed_parallel_and_plugin_are_omitted() {
        let mut r = parallel(10, vec![(11, vec![plug(2)])]);
        r.set_bypassed(true);
        let mut p = plug(3);
        p.set_bypassed(true);
        let devices = vec![plug(1), r, p];
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &HashSet::new(), RenderScope::Mix, 48_000);
        assert_eq!(op_kinds(&b.program), vec!["P1"]);
    }

    #[test]
    fn paraout_split_lands_after_the_top_level_device() {
        let devices = vec![plug(1), parallel(10, vec![(11, vec![plug(2)])]), plug(3)];
        let b = build_program(&devices, 7, Some(2), &DeviceLatencies::new(), &HashSet::new(), RenderScope::Mix, 48_000);
        // P1 RB CB P2 CE RE | P3
        assert_eq!(b.program.pass1_end, 6);
    }

    /// r.md #112: `Frequency3` の Parallel は chain 1/2/3 に出力 0/1/2 (Low/Mid/High)、 4 本目は
    /// 素通し。 `MidSide` は 2 出力。 scratch に分割器が置かれ、 `Split::None` には置かれない。
    #[test]
    fn split_assigns_outputs_by_chain_order_and_allocates_the_splitter() {
        let mut split = parallel(10, vec![(11, vec![]), (12, vec![]), (13, vec![]), (14, vec![])]);
        split.as_parallel_mut().unwrap().split = common::model::Split::DEFAULT_FREQUENCY3;
        let mut ms = parallel(30, vec![(31, vec![]), (32, vec![]), (33, vec![])]);
        ms.as_parallel_mut().unwrap().split = common::model::Split::MidSide;
        let plain = parallel(20, vec![(21, vec![])]);
        // r.md #114: Selector は全 chain が出力 (chain 数に追従)。
        let mut sel = parallel(40, vec![(41, vec![]), (42, vec![]), (43, vec![])]);
        sel.as_parallel_mut().unwrap().split = common::model::Split::DEFAULT_SELECTOR;
        let b = build_program(&[split, ms, plain, sel], 7, None, &DeviceLatencies::new(), &HashSet::new(), RenderScope::Mix, 48_000);
        let outputs: Vec<Option<u8>> = b
            .program
            .ops
            .iter()
            .filter_map(|op| match op {
                ChainOp::ChainBegin { output, .. } => Some(*output),
                _ => None,
            })
            .collect();
        assert_eq!(
            outputs,
            vec![Some(0), Some(1), Some(2), None, Some(0), Some(1), None, None, Some(0), Some(1), Some(2)]
        );
        assert!(b.program.parallels[0].split.is_some());
        assert!(b.program.parallels[1].split.is_some());
        assert!(b.program.parallels[2].split.is_none());
        assert!(b.program.parallels[3].split.is_some());
        assert_eq!(b.chain_slots[&12].output, Some(1));
        assert_eq!(b.chain_slots[&14].output, None);
    }

    /// `RenderScope::Sources` の program: 音声入力を持つ plugin と内蔵 device の op を出さず (latency も数えない)、
    /// 音源 (音声入力を持たない plugin) は残す。Parallel は素材の音だけを描く形になり (帯域分割は置かず、Selector は
    /// MIDI の配り方として残す)、音源 chain の latency に入力を揃える遅延を持つ。フェーダーは掛けない。
    #[test]
    fn sources_scope_omits_processing_devices_and_bakes_the_parallel_shape() {
        use common::model::{NativeDevice, NativeKind, Split};
        use common::port_config::PortConfig;
        let with_ports = |id: u64, ports: PortConfig| {
            Device::Plugin(PluginInstance { id, ..PluginInstance::with_ports(format!("p{id}"), PluginFormat::Clap, ports) })
        };
        let fx = |id| with_ports(id, PortConfig { has_audio_input: true, has_audio_output: true, ..PortConfig::default() });
        let synth = |id| with_ports(id, PortConfig { has_note_input: true, has_audio_output: true, ..PortConfig::default() });
        let mut bands = parallel(10, vec![(11, vec![synth(3), fx(4)]), (12, vec![fx(5)]), (13, vec![])]);
        bands.as_parallel_mut().unwrap().split = Split::DEFAULT_FREQUENCY3;
        let mut selector = parallel(20, vec![(21, vec![synth(6)]), (22, vec![])]);
        selector.as_parallel_mut().unwrap().split = Split::DEFAULT_SELECTOR;
        let devices = vec![
            synth(1),
            fx(2),
            Device::Native(NativeDevice::new_builtin(NativeKind::Comp, 30)),
            bands,
            selector,
        ];
        let lat: DeviceLatencies = [(1, 7), (2, 100), (3, 40), (4, 1000), (5, 500), (6, 0)].into();

        let mix = build_program(&devices, 7, None, &lat, &HashSet::new(), RenderScope::Mix, 48_000);
        assert_eq!(mix.latency, 7 + 100 + 1040, "前提: Mix は全部数える");
        assert!(mix.program.fader && !mix.program.parallels[0].sources);

        let b = build_program(&devices, 7, None, &lat, &HashSet::new(), RenderScope::Sources, 48_000);
        assert_eq!(
            op_kinds(&b.program),
            vec!["P1", "RB0", "CB0", "P3", "CE0", "CB1", "CE1d40", "CB2", "CE2d40", "RE0", "RB1", "CB3", "P6", "CE3", "CB4", "CE4", "RE1"],
            "fx と内蔵 device の op は出ない"
        );
        assert_eq!(b.latency, 7 + 40, "出さない device の latency は数えない");
        assert!(!b.program.fader, "フェーダーを掛けない");
        assert!(b.program.natives.is_empty());
        assert!(b.program.parallels.iter().all(|r| r.sources));
        assert!(b.program.parallels[0].split.is_none(), "帯域分割は置かない");
        assert!(b.program.parallels[1].split.is_some(), "Selector は MIDI の配り方として残す");
        let input_delays: Vec<Option<u32>> = b
            .program
            .ops
            .iter()
            .filter_map(|op| match op {
                ChainOp::ParallelEnd { input_delay, .. } => Some(input_delay.map(|(_, frames)| frames)),
                _ => None,
            })
            .collect();
        assert_eq!(input_delays, vec![Some(40), None], "入力を音源 chain の latency に揃える (0 なら無し)");
        assert!(b.program.delay_keys.contains(&10), "入力の遅延は Parallel id で状態を移送する");
    }

    #[test]
    fn tap_needs_are_baked_into_chain_end() {
        let devices = vec![parallel(10, vec![(11, vec![]), (12, vec![])])];
        let taps: HashSet<(u64, TapPoint)> = [(11, TapPoint::PostFx), (12, TapPoint::PostFader)].into();
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &taps, RenderScope::Mix, 48_000);
        let ends: Vec<(bool, bool)> = b
            .program
            .ops
            .iter()
            .filter_map(|op| match op {
                ChainOp::ChainEnd { snapshot_post_fx, snapshot_post_fader, .. } => {
                    Some((*snapshot_post_fx, *snapshot_post_fader))
                }
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![(true, false), (false, true)]);
    }
}
