//! `ChainProgram` の compile 側 (off-RT): device ツリー → 命令列 + scratch + 並列 PDC。
//! `docs/plan_parallel.md` §4.1 / §4.2。

use std::collections::{HashMap, HashSet};

use common::model::{Device, SplitBand, TapPoint, TapSource};

use super::compile::DeviceLatencies;
use super::delay_line::DelayLine;
use super::program::{ChainOp, ChainProgram, ChainScratch, ParallelScratch};

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
}

/// chain の program 内の位置。 `band` = r.md #112 帯域分割でこの chain が受ける帯域
/// (`PreFx` tap はその帯域の出力を指す)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainSlot {
    pub chain_slot: u32,
    pub parallel_slot: u32,
    pub band: Option<SplitBand>,
}

/// `devices` (bypass 中は除く) の直列 latency。Parallel は chain の最大。
pub fn program_latency(devices: &[Device], latencies: &DeviceLatencies) -> u32 {
    devices.iter().fold(0u32, |acc, d| match d {
        Device::Plugin(p) => {
            if p.bypassed {
                acc
            } else {
                acc.saturating_add(latencies.get(&p.id).copied().unwrap_or(0))
            }
        }
        Device::Parallel(r) => {
            if r.bypassed {
                acc
            } else {
                acc.saturating_add(
                    r.chains
                        .iter()
                        .map(|c| program_latency(&c.devices, latencies))
                        .max()
                        .unwrap_or(0),
                )
            }
        }
    })
}

/// `devices` を展開する。`split_top` = パラアウトの top-level split index
/// (`Track::paraout_split_device`、`None` = 全部 pass 1)。`taps` = 誰かが読む
/// `(chain_id, tap_point)` の集合 (snapshot flag に焼く)。
pub fn build_program(
    devices: &[Device],
    track_id: u32,
    split_top: Option<u32>,
    latencies: &DeviceLatencies,
    taps: &HashSet<(u64, TapPoint)>,
) -> BuiltProgram {
    let mut program = ChainProgram::empty(track_id);
    let mut chain_latency: HashMap<u64, ChainLatency> = HashMap::new();
    let mut chain_slots: HashMap<u64, ChainSlot> = HashMap::new();
    let mut b = Builder {
        program: &mut program,
        latencies,
        taps,
        chain_latency: &mut chain_latency,
        chain_slots: &mut chain_slots,
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
    BuiltProgram {
        program,
        latency: acc,
        chain_latency,
        chain_slots,
    }
}

struct Builder<'a> {
    program: &'a mut ChainProgram,
    latencies: &'a DeviceLatencies,
    taps: &'a HashSet<(u64, TapPoint)>,
    chain_latency: &'a mut HashMap<u64, ChainLatency>,
    chain_slots: &'a mut HashMap<u64, ChainSlot>,
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
                if p.bypassed {
                    return 0;
                }
                self.program.ops.push(ChainOp::Plugin {
                    device_id: p.id,
                    ports: p.ports,
                    own_prefx_ports: own_prefx_ports(p, self.program.track_id),
                });
                self.latencies.get(&p.id).copied().unwrap_or(0)
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
    fn emit_parallel(&mut self, r: &common::model::Parallel, prefix: u32) -> u32 {
        let parallel_slot = self.program.parallels.len() as u32;
        self.program.parallels.push(ParallelScratch::new(r.id, !r.split.is_none()));
        self.program.ops.push(ChainOp::ParallelBegin { parallel_slot });
        let max = r
            .chains
            .iter()
            .map(|c| program_latency(&c.devices, self.latencies))
            .max()
            .unwrap_or(0);
        for (k, c) in r.chains.iter().enumerate() {
            let chain_slot = self.program.chains.len() as u32;
            let band = r.split.band_of(k);
            self.program.chains.push(ChainScratch::new(c.id));
            self.chain_slots.insert(c.id, ChainSlot { chain_slot, parallel_slot, band });
            self.program.ops.push(ChainOp::ChainBegin { parallel_slot, chain_slot, band });
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
        self.program.ops.push(ChainOp::ParallelEnd { parallel_slot, parallel_id: r.id });
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
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &HashSet::new());
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
        let b = build_program(&devices, 7, None, &lat, &HashSet::new());
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
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &HashSet::new());
        assert_eq!(op_kinds(&b.program), vec!["P1"]);
    }

    #[test]
    fn paraout_split_lands_after_the_top_level_device() {
        let devices = vec![plug(1), parallel(10, vec![(11, vec![plug(2)])]), plug(3)];
        let b = build_program(&devices, 7, Some(2), &DeviceLatencies::new(), &HashSet::new());
        // P1 RB CB P2 CE RE | P3
        assert_eq!(b.program.pass1_end, 6);
    }

    /// r.md #112: `Frequency3` の Parallel は chain 1/2/3 に Low/Mid/High、 4 本目は全帯域。
    /// scratch に分割器が置かれ、 `Split::None` の Parallel には置かれない。
    #[test]
    fn frequency_split_assigns_bands_by_chain_order_and_allocates_the_splitter() {
        let mut split = parallel(10, vec![(11, vec![]), (12, vec![]), (13, vec![]), (14, vec![])]);
        split.as_parallel_mut().unwrap().split = common::model::Split::DEFAULT_FREQUENCY3;
        let plain = parallel(20, vec![(21, vec![])]);
        let b = build_program(&[split, plain], 7, None, &DeviceLatencies::new(), &HashSet::new());
        let bands: Vec<Option<SplitBand>> = b
            .program
            .ops
            .iter()
            .filter_map(|op| match op {
                ChainOp::ChainBegin { band, .. } => Some(*band),
                _ => None,
            })
            .collect();
        assert_eq!(
            bands,
            vec![Some(SplitBand::Low), Some(SplitBand::Mid), Some(SplitBand::High), None, None]
        );
        assert!(b.program.parallels[0].split.is_some());
        assert!(b.program.parallels[1].split.is_none());
        assert_eq!(b.chain_slots[&12].band, Some(SplitBand::Mid));
        assert_eq!(b.chain_slots[&14].band, None);
    }

    #[test]
    fn tap_needs_are_baked_into_chain_end() {
        let devices = vec![parallel(10, vec![(11, vec![]), (12, vec![])])];
        let taps: HashSet<(u64, TapPoint)> = [(11, TapPoint::PostFx), (12, TapPoint::PostFader)].into();
        let b = build_program(&devices, 7, None, &DeviceLatencies::new(), &taps);
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
