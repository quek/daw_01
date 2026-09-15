use super::*;
use crate::graph::DelayKey;
use common::model::{Device, PluginInstance, Song, Track};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;

/// PDC テスト用: `samples` を報告する device を 1 個持つ chain を作り、
/// 報告値を `lat` に登録する。 報告 latency は `Song` ではなく
/// `DeviceLatencies` 側に居る (r.md #9) ので、 テストも同じ形で組む。
fn latency_chain(
    lat: &mut DeviceLatencies,
    device_id: u64,
    samples: u32,
) -> Vec<Device> {
    lat.insert(device_id, samples);
    vec![Device::Plugin(PluginInstance {
        id: device_id,
        ..PluginInstance::with_ports(
            format!("test.latent{device_id}"),
            PluginFormat::Clap,
            audio_fx_ports(),
        )
    })]
}

/// v23 single-chain: `Track::default()` を mutator で埋める helper。
/// downstream crate (daw_audio) の test で `Track { .., ..Track::default() }`
/// を書くと、 `common` 内の `pub(crate)` legacy migration fields が見えず
/// E0451 になるため、 private field に触れない default + mutate で回避する。
fn track(f: impl FnOnce(&mut Track)) -> Track {
    let mut t = Track::default();
    f(&mut t);
    t
}

#[test]
fn bypassed_device_is_excluded_from_pdc_and_sidechain_taps() {
    // r.md #105: bypass 中の device は dispatch されないので、報告 latency を
    // PDC に数えず、sidechain tap も staging しない。
    use common::plugin_format::PluginFormat;

    let mut lat = DeviceLatencies::new();
    let mut latent = latency_chain(&mut lat, 20, 2048);
    latent[0].set_bypassed(true);
    let song = Song {
        tracks: vec![
            track(|t| t.id = 1),
            track(|t| {
                t.id = 2;
                t.devices = latent;
            }),
            track(|t| {
                t.id = 3;
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 77,
                    bypassed: true,
                    aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                    ..PluginInstance::with_ports(
                        "test.compressor".into(),
                        PluginFormat::Vst3,
                        audio_fx_ports(),
                    )
                })];
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();
    assert_eq!(sched.master_latency_samples, 0, "bypass 中の 2048 sample は数えない");
    assert!(
        !sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::SidechainTap { device_id: 77, .. })),
        "bypass 中の device へ SidechainTap を emit しない: nodes={:?}",
        sched.nodes
    );

    // 同じ song で bypass を外すと両方が復活する (= 判定がフラグ由来であることの対照)。
    let mut live = song;
    live.tracks[1].devices[0].set_bypassed(false);
    live.tracks[2].devices[0].set_bypassed(false);
    let sched = compile_schedule(&live, &lat, 48_000, 0, RenderScope::Mix).unwrap();
    assert_eq!(sched.master_latency_samples, 2048);
    assert!(sched
        .nodes
        .iter()
        .any(|op| matches!(op, NodeOp::SidechainTap { device_id: 77, .. })));
}

/// v23 single-chain: a pure audio-FX device (audio output only, no note
/// I/O) — derives as `AudioEffect` when no device in the chain has note
/// input. Used by the sidechain / PDC tests where the dest plugin is a
/// compressor on the track's audio signal.
fn audio_fx_ports() -> PortConfig {
    PortConfig {
        has_note_input: false,
        has_note_output: false,
        has_audio_output: true,
        // pure audio-FX: audio を加工する → audio 入力あり。
        has_audio_input: true,
        has_video_input: false,
        has_video_output: false,
    }
}

/// v23 single-chain: an instrument device (note input + audio output) —
/// derives as `Instrument` (MIDI→audio). Used by the instrument-sidechain
/// test.
fn instrument_ports() -> PortConfig {
    PortConfig {
        has_note_input: true,
        has_note_output: false,
        has_audio_output: true,
        // instrument: note→audio 生成。 audio を加工しない → audio 入力なし。
        has_audio_input: false,
        has_video_input: false,
        has_video_output: false,
    }
}

/// r.md #129 §15.3 T4: サイドチェインを読む consumer の種類。SC の会計 (tap の emit / 依存辺 /
/// input delay / path latency / 循環) は plugin の aux 入力と内蔵 device の SC で同じ規則なので、
/// SC のテストは両方で回す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScConsumer {
    Plugin,
    Native,
}

const SC_CONSUMERS: [ScConsumer; 2] = [ScConsumer::Plugin, ScConsumer::Native];

/// id `id` の consumer (plugin は audio FX、native は追加の Comp)。`source` track の PostFader を読む。
fn sc_consumer(kind: ScConsumer, id: u64, source: u32) -> Device {
    let route = common::model::AuxInputRoute::post_fader(source);
    match kind {
        ScConsumer::Plugin => Device::Plugin(PluginInstance {
            id,
            aux_inputs: vec![Some(route)],
            ..PluginInstance::with_ports("test.compressor".into(), PluginFormat::Vst3, audio_fx_ports())
        }),
        ScConsumer::Native => Device::Native(common::model::NativeDevice {
            aux_input: Some(route),
            ..common::model::NativeDevice::new_added(common::model::NativeKind::Comp, id, 1)
        }),
    }
}

/// consumer `id` への tap の位置 (nodes の index)。native は owner の program の受け皿が
/// `Staged` で確保済みであることまで確かめる (= tap を出した場所で受け皿を用意した)。
fn sc_tap_position(sched: &Schedule, kind: ScConsumer, src: BufRef, id: u64, owner: u32) -> Option<usize> {
    sched.nodes.iter().position(|op| match (kind, op) {
        (ScConsumer::Plugin, NodeOp::SidechainTap { src: s, device_id, aux_in_port: 0 }) => {
            *s == src && *device_id == id
        }
        (ScConsumer::Native, NodeOp::NativeSidechainTap { src: s, owner: o, native_slot }) => {
            let p = if owner == MASTER_OWNER { &sched.master_program } else { &sched.track_programs[owner as usize] };
            *s == src
                && *o == owner
                && p.natives.get(*native_slot as usize).is_some_and(|ns| {
                    ns.device_id == id && ns.sc_mode == crate::graph::native::ScMode::Staged && ns.sc.is_some()
                })
        }
        _ => false,
    })
}

fn any_sc_tap(sched: &Schedule) -> bool {
    sched.nodes.iter().any(|op| matches!(op, NodeOp::SidechainTap { .. } | NodeOp::NativeSidechainTap { .. }))
}

#[test]
fn empty_song_compiles_to_master_only_mix() {
    let song = Song::default();
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
    assert_eq!(sched.nodes.len(), 1);
    match &sched.nodes[0] {
        NodeOp::Mix { dst, srcs } => {
            assert_eq!(*dst, BufRef::Master);
            assert!(srcs.is_empty());
        }
        other => panic!("expected Mix → Master, got {other:?}"),
    }
}

#[test]
fn tap_bufref_resolves_three_tap_points() {
    use common::model::TapPoint;
    // docs/plan_modulation_followups.md §1: PostFader=TrackScratch,
    // PostFx=PreFaderScratch, PreFx=専用 PreFxScratch (旧フォールバック撤廃)。
    assert_eq!(tap_bufref(TapPoint::PostFader, 3), BufRef::TrackScratch(3));
    assert_eq!(tap_bufref(TapPoint::PostFx, 3), BufRef::PreFaderScratch(3));
    assert_eq!(tap_bufref(TapPoint::PreFx, 3), BufRef::PreFxScratch(3));
}

#[test]
fn flat_audio_tracks_emit_process_then_mix() {
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
            }),
            track(|t| {
                t.id = 2;
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
    // Depth ordering is stable for a flat song, so ProcessTrack ops
    // appear in reverse-track-order (depth=0 group is just descending
    // index order); both ways the Mix at the end carries both refs.
    assert_eq!(sched.nodes.len(), 3);
    let process_count = sched
        .nodes
        .iter()
        .filter(|op| matches!(op, NodeOp::ProcessTrack { .. }))
        .count();
    assert_eq!(process_count, 2);
    match sched.nodes.last().unwrap() {
        NodeOp::Mix { dst, srcs } => {
            assert_eq!(*dst, BufRef::Master);
            assert_eq!(srcs.len(), 2);
            let track_indices: Vec<u32> = srcs
                .iter()
                .map(|(b, _)| match b {
                    BufRef::TrackScratch(i) => *i,
                    other => panic!("unexpected master src {other:?}"),
                })
                .collect();
            assert!(track_indices.contains(&0));
            assert!(track_indices.contains(&1));
        }
        other => panic!("expected Mix → Master, got {other:?}"),
    }
}

#[test]
fn group_with_children_compiles_to_two_phase_mix() {
    // Layout:
    //   Group 1 (Drums) → Master
    //     ├─ Audio 2 (Kick)   parent=1
    //     └─ Audio 3 (Snare)  parent=1
    //   Audio 4 (Lead) → Master  (no parent)
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Drums".into();
                t.parent_group_id = None;
            }),
            track(|t| {
                t.id = 2;
                t.name = "Kick".into();
                t.parent_group_id = Some(1);
            }),
            track(|t| {
                t.id = 3;
                t.name = "Snare".into();
                t.parent_group_id = Some(1);
            }),
            track(|t| {
                t.id = 4;
                t.name = "Lead".into();
                t.parent_group_id = None;
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

    // Expect (in some order): two leaf ProcessTrack (kick, snare),
    // group's Mix-into-self + ProcessGroupFx, lead ProcessTrack, then
    // final Mix → Master with two refs (group + lead).
    let process_track_idxs: Vec<u32> = sched
        .nodes
        .iter()
        .filter_map(|op| match op {
            NodeOp::ProcessTrack { track_idx } => Some(*track_idx),
            _ => None,
        })
        .collect();
    assert!(process_track_idxs.contains(&1)); // Kick at index 1
    assert!(process_track_idxs.contains(&2)); // Snare at index 2
    assert!(process_track_idxs.contains(&3)); // Lead at index 3

    // Group must be processed *after* its children → kick and snare
    // ProcessTrack must come before Group's Mix-into-self.
    let kick_pos = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 1 }))
        .unwrap();
    let snare_pos = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 2 }))
        .unwrap();
    let group_mix_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::TrackScratch(0),
                    ..
                }
            )
        })
        .unwrap();
    assert!(
        kick_pos < group_mix_pos,
        "kick must process before group mix"
    );
    assert!(
        snare_pos < group_mix_pos,
        "snare must process before group mix"
    );

    // Group's Mix should carry both children.
    let group_mix_srcs = sched
        .nodes
        .iter()
        .find_map(|op| match op {
            NodeOp::Mix {
                dst: BufRef::TrackScratch(0),
                srcs,
            } => Some(srcs.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(group_mix_srcs.len(), 2);

    // Master mix must reference *the group* (idx 0) and *Lead* (idx 3),
    // not the raw children (which are folded into the group bus).
    let master_srcs = sched
        .nodes
        .iter()
        .find_map(|op| match op {
            NodeOp::Mix {
                dst: BufRef::Master,
                srcs,
            } => Some(srcs.clone()),
            _ => None,
        })
        .unwrap();
    let master_idxs: Vec<u32> = master_srcs
        .iter()
        .map(|(b, _)| match b {
            BufRef::TrackScratch(i) => *i,
            other => panic!("unexpected master src {other:?}"),
        })
        .collect();
    assert!(master_idxs.contains(&0)); // Group
    assert!(master_idxs.contains(&3)); // Lead
    assert!(!master_idxs.contains(&1));
    assert!(!master_idxs.contains(&2));
}

#[test]
fn nested_groups_emit_inner_group_before_outer() {
    // Outer track 1 (becomes a group because track 2 points at it)
    //   Inner track 2 (becomes a group because track 3 points at it)
    //     Audio 3 (parent=2)
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.parent_group_id = None;
            }),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
            }),
            track(|t| {
                t.id = 3;
                t.parent_group_id = Some(2);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

    let audio_pos = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 2 }))
        .unwrap();
    let inner_mix_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::TrackScratch(1),
                    ..
                }
            )
        })
        .unwrap();
    let outer_mix_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::TrackScratch(0),
                    ..
                }
            )
        })
        .unwrap();
    assert!(audio_pos < inner_mix_pos);
    assert!(inner_mix_pos < outer_mix_pos);
}

#[test]
fn parent_cycle_is_rejected() {
    // Track 1 ↔ Track 2 cycle.
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.parent_group_id = Some(2);
            }),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
            }),
        ],
        ..Song::default()
    };
    assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
}

#[test]
fn audio_track_can_become_a_group_implicitly() {
    // With kind removed, any track that has a child IS a group.
    // Track 1 has no flag, but track 2 points at it via
    // parent_group_id, so track 1 is treated as a group bus.
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
            }),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
    // Track 1 should emit Mix → TrackScratch(0) + ProcessGroupFx(0),
    // not ProcessTrack(0).
    let has_group_fx = sched
        .nodes
        .iter()
        .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 0, .. }));
    assert!(has_group_fx, "track 1 must be treated as a group");
    let has_process_track_0 = sched
        .nodes
        .iter()
        .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }));
    assert!(
        !has_process_track_0,
        "track 1 has children so its leaf op should be skipped"
    );
}

#[test]
fn parent_pointing_to_unknown_track_is_rejected() {
    let song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.parent_group_id = Some(99);
        })],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule_for_test(&song, 48_000, 0).err(),
        Some(GraphError::DanglingReference(99))
    );
}

// ---- PR3: Plugin Delay Compensation ----

/// r.md #39: `master_latency_samples` = master 合流点で全 src が揃えられる
/// latency。engine はこの値だけ metronome click の参照位置を戻して、遅延
/// プラグインを通った track の音と click を一致させる。
#[test]
fn master_latency_samples_is_the_max_path_latency_reaching_master() {
    // latency 無しなら 0 (= 補償不要)。
    let plain = Song {
        tracks: vec![track(|t| t.id = 1)],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule_for_test(&plain, 48_000, 0).unwrap().master_latency_samples,
        0
    );

    // 直列でない 2 track のうち片方が 2048 sample 報告 → master は 2048 で揃う。
    let mut lat = DeviceLatencies::new();
    let latent = Song {
        tracks: vec![
            track(|t| t.id = 1),
            track(|t| {
                t.id = 2;
                t.devices = latency_chain(&mut lat, 20, 2048);
            }),
        ],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&latent, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        2048
    );

    // group 経由 (= 親を指す子がいる) でも、子の latency が group の path latency
    // として master へ伝わる。master src は group 1 本だけ。
    let mut lat = DeviceLatencies::new();
    let grouped = Song {
        tracks: vec![
            track(|t| t.id = 1),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
                t.devices = latency_chain(&mut lat, 20, 512);
            }),
        ],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&grouped, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        512
    );

    // マスターリミッター ON はルックアヘッド (5ms = 48kHz で 240) を出力遅延として足す。
    // OFF (既定 = 上の全ケース) は足さない。
    let mut limited = Song {
        tracks: vec![track(|t| t.id = 1)],
        ..Song::default()
    };
    limited.master_limiter.on = true;
    assert_eq!(
        compile_schedule_for_test(&limited, 48_000, 0).unwrap().master_latency_samples,
        common::model::limiter_lookahead_samples(48_000),
        "リミッター ON のルックアヘッドは master 出力の遅延"
    );
    assert_eq!(common::model::limiter_lookahead_samples(48_000), 240);
}

/// r.md #129 §15.3 T8: Limiter の遅延は compile 時に焼く。静的に OFF でも On のレーン (や変調) があれば
/// 遅延を焼き、PDC の会計 (`master_latency_samples`) と DSP (`master_limiter_latency`) が同じ値を見る
/// (以前は On をオートメーションすると遅延と会計が食い違った = §18-G)。track が 0 本の早期 return でも同じ。
#[test]
fn master_limiter_latency_is_baked_from_the_static_on_or_an_on_lane() {
    use common::model::{AutomationLane, AutomationTarget, MasterLimiterParam, ModRouting, Polarity};
    let on = AutomationTarget::MasterLimiter(MasterLimiterParam::On);
    let look = common::model::limiter_lookahead_samples(48_000);
    for tracks in [vec![track(|t| t.id = 1)], vec![]] {
        let plain = Song { tracks: tracks.clone(), ..Song::default() };
        let sched = compile_schedule_for_test(&plain, 48_000, 0).unwrap();
        assert_eq!((sched.master_limiter_latency, sched.master_latency_samples), (false, 0), "静的 OFF・レーン無し");

        let laned = Song { tracks: tracks.clone(), song_lanes: vec![AutomationLane::new(on.clone(), 0.0)], ..Song::default() };
        let sched = compile_schedule_for_test(&laned, 48_000, 0).unwrap();
        assert_eq!((sched.master_limiter_latency, sched.master_latency_samples), (true, look), "On レーンで遅延を焼く");

        let mut disabled = laned.clone();
        disabled.song_lanes[0].enabled = false;
        assert!(!compile_schedule_for_test(&disabled, 48_000, 0).unwrap().master_limiter_latency, "無効なレーンは数えない");

        let routed = Song {
            tracks,
            song_mod_routings: vec![ModRouting {
                id: 1,
                target: on.clone(),
                source_id: 3,
                depth: 1.0,
                polarity: Polarity::Unipolar,
                enabled: true,
            }],
            ..Song::default()
        };
        assert!(compile_schedule_for_test(&routed, 48_000, 0).unwrap().master_limiter_latency, "On への変調でも焼く");
    }
}

/// `RenderScope` の段の有無は schedule の形に焼く: `Mix` だけが master の段 (fx chain / Limiter の遅延) を持ち、
/// トラックのフェーダーを持ち、`Sources` だけがトラックの fx を外す。master の段を通さない scope は master fx と
/// Limiter の遅延を出力遅延に数えない (bounce の書き出し窓がずれない)。
#[test]
fn render_scope_shapes_the_master_stage_and_track_faders() {
    let mut lat = DeviceLatencies::new();
    let mut song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.devices = latency_chain(&mut lat, 20, 512);
        })],
        master_fx_chain: latency_chain(&mut lat, 90, 2048),
        ..Song::default()
    };
    song.master_limiter.on = true;
    let look = common::model::limiter_lookahead_samples(48_000);
    let shape = |scope| {
        let s = compile_schedule(&song, &lat, 48_000, 0, scope).unwrap();
        (s.master_stage, s.master_program.ops.len(), s.master_limiter_latency, s.master_latency_samples, s.track_programs[0].fader)
    };
    assert_eq!(shape(RenderScope::Mix), (true, 1, true, 512 + 2048 + look, true));
    assert_eq!(shape(RenderScope::PostFx), (false, 0, false, 512, false), "PostFx 点まで (fx は数え、フェーダーは掛けない)");
    assert_eq!(shape(RenderScope::Sources), (false, 0, false, 0, false), "音声入力を持つ fx も数えない");
}

/// r.md #39: `master_latency_samples` は master **出力** の遅延量なので、
/// send/return・パラアウト・master fx のどの経路で latency が入っても拾う。
/// (レビュー指摘: 並列 2 track と group しか見ていなかった。)
#[test]
fn master_latency_samples_covers_send_paraout_and_master_fx() {
    use common::model::{AuxOutputRoute, PluginInstance, Send, SendMode};
    use common::plugin_format::PluginFormat;

    // (a) send/return: Vocal → Reverb(latency 100)。return が master src なので
    //     master 合流は 100 に揃う。
    let mut lat = DeviceLatencies::new();
    let sends = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 2,
                    gain: 0.5,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.devices = latency_chain(&mut lat, 20, 100);
            }),
        ],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&sends, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        100,
        "send 先 (return) の latency も master 合流に効く"
    );

    // (b) パラアウト: Drums.aux → Snare(latency 256)。dest は独立 bus なので
    //     source の path latency を取り込んだ上で自分の chain latency を足す。
    let mut lat = DeviceLatencies::new();
    lat.insert(10, 64);
    let paraout = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 10,
                    aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                    aux_output_count: 1,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        instrument_ports(),
                    )
                })];
            }),
            track(|t| {
                t.id = 4;
                t.devices = latency_chain(&mut lat, 40, 256);
            }),
        ],
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&paraout, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        64 + 256,
        "paraout dest は source の path latency を取り込む"
    );

    // (c) master fx: track 側 latency 0 でも master chain の報告 latency を拾う。
    //     旧実装は master Mix の src だけを見ていたので 0 のまま = click が先行した。
    let mut lat = DeviceLatencies::new();
    let master_fx = Song {
        tracks: vec![track(|t| t.id = 1)],
        master_fx_chain: latency_chain(&mut lat, 90, 2048),
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&master_fx, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        2048,
        "master fx chain の latency も master 出力の遅延"
    );

    // (d) track 側と master fx は加算 (直列なので両方遅れる)。
    let mut lat = DeviceLatencies::new();
    let both = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = latency_chain(&mut lat, 20, 512);
            }),
            track(|t| t.id = 2),
        ],
        master_fx_chain: latency_chain(&mut lat, 90, 2048),
        ..Song::default()
    };
    assert_eq!(
        compile_schedule(&both, &lat, 48_000, 0, RenderScope::Mix).unwrap().master_latency_samples,
        512 + 2048
    );
}

/// Compile-level test: 親子無しの 2 track が並行に master へ流れるとき、
/// 片方のみが latency 100 を report していたら、 もう片方 (latency 0)
/// に対して `ApplyDelay { frames: 100 }` を Master Mix の **直前** に
/// 挿入し、 必要な DelayLine を `Schedule::delay_lines` に確保すべき。
///
/// 仕様根拠: Ardour `libs/ardour/route.cc::process_output_buffers` —
/// 各ルート内の effective_latency を直列加算し、 sink (Master) で全
/// path を最大値に揃えるため、 latency が小さい path に DelayLine を
/// 挿入する。
#[test]
fn pdc_parallel_tracks_emit_compensating_delay_for_lower_latency_path() {
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Clean".into();
            }),
            track(|t| {
                t.id = 2;
                t.name = "Latent".into();
                t.devices = latency_chain(&mut lat, 20, 100);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    // (a) DelayLine が 1 本以上、 capacity ≥ 100 で確保されている。
    assert!(
        sched
            .delay_lines
            .iter()
            .any(|dl| dl.capacity() >= 100),
        "compile_schedule must allocate a DelayLine for the laggard's compensation; \
         got delay_lines.len()={}",
        sched.delay_lines.len()
    );

    // (b) Master Mix の **直前** に latency=0 path (TrackScratch(0)) へ
    //     ApplyDelay { frames: 100 } が刺さっている。
    let master_mix_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::Master,
                    ..
                }
            )
        })
        .expect("Master Mix must exist");
    let apply_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::ApplyDelay {
                    buf: BufRef::TrackScratch(0),
                    frames: 100,
                    ..
                }
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "ApplyDelay {{ buf: TrackScratch(0), frames: 100 }} must be inserted \
                 before Master Mix; got nodes={:?}",
                sched.nodes
            )
        });
    assert!(
        apply_pos < master_mix_pos,
        "ApplyDelay must come before Master Mix"
    );

    // (c) latency が大きい側 (TrackScratch(1)) には DelayLine 不要 —
    //     こちらは「全 path の max」と同じ累積 latency を持つので、
    //     compensation を入れると余計な遅延になる。
    let laggard_has_apply = sched.nodes.iter().any(|op| {
        matches!(
            op,
            NodeOp::ApplyDelay {
                buf: BufRef::TrackScratch(1),
                ..
            }
        )
    });
    assert!(
        !laggard_has_apply,
        "the highest-latency path should NOT receive an ApplyDelay"
    );
}

/// 数値テスト: 各 track に「latency を持つ plugin」 をロードした状態で
/// 同一の impulse を input すると、 PDC 無しでは plugin の遅延だけ
/// master の合流点で時間がずれる (= 「トラック間の音ずれ」)。 PDC が
/// 効いていれば、 低 latency path が補償されて master 上の単一 peak
/// に収束する。
///
/// 構成:
///   Track A (id=1) ← LatencyPlugin(0)   identity
///   Track B (id=2) ← LatencyPlugin(100) input を 100 sample 遅延
///   両 track に impulse @sample 0 を入力 → master へ合流
///
/// 期待:
///   PDC OK → master_l[100] ≈ 2.0、 他は 0
///   PDC NG → master_l[0]   ≈ 1.0  (A だけ即時) , master_l[100] ≈ 1.0 (B 遅延)
///            → これが「音ずれ」 で、 本テストはこの状況を assertion で検出
#[test]
fn pdc_two_track_impulse_aligns_at_master_with_loaded_latency_plugin() {
    const FRAMES: usize = 256;

    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "A".into();
            }),
            track(|t| {
                t.id = 2;
                t.name = "B".into();
                t.devices = latency_chain(&mut lat, 20, 100);
            }),
        ],
        ..Song::default()
    };
    let mut sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    // Track ごとに「ロードされた plugin」 を持たせる。 production の
    // CLAP/VST3 と違って format-agnostic な test stub だが、
    //   - state を持つ (history ring buffer)
    //   - process(input -> output) に latency 分の遅延を入れる
    // という意味で「latency を持つ loaded plugin」 そのもの。 Track の
    // 並び (idx) → plugin のマップで保持する。
    let mut plugins: Vec<LatencyPlugin> =
        vec![LatencyPlugin::new(0), LatencyPlugin::new(100)];

    // 各 track の scratch (stereo)。 ProcessTrack ハンドラで
    // plugin.process(input) を呼んだ結果を書き込む。
    let mut scratch_l: Vec<Vec<f32>> = vec![vec![0.0; FRAMES]; 2];
    let mut scratch_r: Vec<Vec<f32>> = vec![vec![0.0; FRAMES]; 2];

    // 共通入力: impulse @sample 0
    let mut input_l = vec![0.0f32; FRAMES];
    let mut input_r = vec![0.0f32; FRAMES];
    input_l[0] = 1.0;
    input_r[0] = 1.0;

    let mut master_l = vec![0.0f32; FRAMES];
    let mut master_r = vec![0.0f32; FRAMES];

    // production の engine.rs:917-968 の dispatch loop を test 用に複製。
    // ProcessTrack で「ロード済 plugin」 の process() を回し、 Mix /
    // ApplyDelay は production と同じロジック。
    for op in &mut sched.nodes {
        match op {
            NodeOp::ProcessTrack { track_idx } => {
                let i = *track_idx as usize;
                plugins[i].process(
                    &input_l,
                    &input_r,
                    &mut scratch_l[i],
                    &mut scratch_r[i],
                );
            }
            NodeOp::ProcessGroupFx { .. }
            | NodeOp::SidechainTap { .. }
            | NodeOp::NativeSidechainTap { .. }
            | NodeOp::MixSend { .. }
            | NodeOp::MixAdditive { .. }
            | NodeOp::ParallelOutTap { .. } => {
                // この test では未使用
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::Master,
            } => {
                for (b, gain) in srcs.iter() {
                    let BufRef::TrackScratch(i) = b else {
                        continue;
                    };
                    let i = *i as usize;
                    for j in 0..FRAMES {
                        master_l[j] += scratch_l[i][j] * gain;
                        master_r[j] += scratch_r[i][j] * gain;
                    }
                }
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::TrackScratch(target_idx),
            } => {
                let target = *target_idx as usize;
                let mut new_l = vec![0.0f32; FRAMES];
                let mut new_r = vec![0.0f32; FRAMES];
                for (b, gain) in srcs.iter() {
                    let BufRef::TrackScratch(i) = b else {
                        continue;
                    };
                    let i = *i as usize;
                    for j in 0..FRAMES {
                        new_l[j] += scratch_l[i][j] * gain;
                        new_r[j] += scratch_r[i][j] * gain;
                    }
                }
                scratch_l[target] = new_l;
                scratch_r[target] = new_r;
            }
            NodeOp::Mix { .. } => {
                // Pooled / PluginAuxOut: PR4
            }
            NodeOp::ApplyDelay {
                buf,
                line_idx,
                frames,
            } => {
                let BufRef::TrackScratch(i) = buf else {
                    continue;
                };
                let i = *i as usize;
                let line = &mut sched.delay_lines[*line_idx as usize];
                let in_l = scratch_l[i].clone();
                let in_r = scratch_r[i].clone();
                line.step(
                    &in_l,
                    &in_r,
                    &mut scratch_l[i],
                    &mut scratch_r[i],
                    *frames as usize,
                );
            }
            NodeOp::EnvelopeFollow { .. } => {
                // followers produce only control-rate scalars; they do
                // not affect the audio output exercised by this test.
            }
        }
        // input は 1 buffer 分だけ消費するので、 2 回目以降は input を
        // 0 で埋める必要は無い (ProcessTrack はループ中に 2 回呼ばれない
        // 想定。 ループは 1 buffer 1 イテレーション)。
        let _ = (&input_l, &input_r);
    }

    // (a) sample 0 には peak が立たない (= Track A の出力が PDC で
    //     100 sample 遅延されて、 sample 0 の地点には何も無い)。
    assert!(
        master_l[0].abs() < 1e-6,
        "master_l[0] should be 0 after PDC, got {} (= track misalignment)",
        master_l[0]
    );

    // (b) sample 100 で 2 track の impulse が重なって peak になる。
    assert!(
        (master_l[100] - 2.0).abs() < 1e-6,
        "master_l[100] should be ~2.0 (both tracks' impulses aligned), got {}",
        master_l[100]
    );

    // (c) sample 100 以外は 0 (= 1 つの peak だけ、 「音ずれ」 なし)。
    for (i, &v) in master_l.iter().enumerate() {
        if i == 100 {
            continue;
        }
        assert!(
            v.abs() < 1e-6,
            "master_l[{}] should be 0, got {} (= track misalignment)",
            i,
            v
        );
    }

    // (d) r.md #39: この peak 位置こそが `master_latency_samples` の定義
    //     — `master_buffer[P]` に載っているのは曲位置 `P - master_latency_samples`。
    //     metronome click の参照位置と WAV 書き出し窓は、どちらもこの値を引くことで
    //     曲位置に揃う (`engine.rs` の click_pos / `export.rs` の
    //     `shift_window_for_master_latency`)。ここが崩れると両方が同時に壊れる。
    assert_eq!(
        sched.master_latency_samples, 100,
        "曲位置 0 の impulse は master[master_latency_samples] に現れる"
    );
}

/// テスト専用「latency を持つ plugin」 stub。 production の `LoadedPlugin`
/// trait は format 固有 (CLAP/VST3) の重い API を必要とするため、 PDC
/// グラフレイヤを単独で検証するためだけの最小 stub を test mod 内に置く。
/// `process(input -> output)` で `latency` サンプルだけ遅延した出力を返す。
struct LatencyPlugin {
    latency: usize,
    hist_l: Vec<f32>,
    hist_r: Vec<f32>,
    write: usize,
    cap: usize,
}

impl LatencyPlugin {
    fn new(latency: usize) -> Self {
        // capacity = latency + 1 で「latency 分の遅延」 を厳密に再現
        // (DelayLine の clamp 仕様と整合)。
        let cap = latency + 1;
        Self {
            latency,
            hist_l: vec![0.0; cap],
            hist_r: vec![0.0; cap],
            write: 0,
            cap,
        }
    }

    fn process(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let n = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
        if self.latency == 0 {
            out_l[..n].copy_from_slice(&in_l[..n]);
            out_r[..n].copy_from_slice(&in_r[..n]);
            return;
        }
        for i in 0..n {
            self.hist_l[self.write] = in_l[i];
            self.hist_r[self.write] = in_r[i];
            let read = (self.write + self.cap - self.latency) % self.cap;
            out_l[i] = self.hist_l[read];
            out_r[i] = self.hist_r[read];
            self.write = (self.write + 1) % self.cap;
        }
    }
}

// ---- PR4 Sidechain ----

/// Compile-level test: Track 2 (index 1) の device 0 (device_id 77) に
/// `aux_inputs = [Some(track_1)]` が設定されているとき、
/// `compile_schedule` は次の順で nodes を emit する。
///
/// 1. `ProcessTrack(0)` (Track 1、 source)
/// 2. `SidechainTap { src: TrackScratch(0), device_id: 77, aux_in_port: 0 }`
/// 3. `ProcessTrack(1)` (Track 2、 receiver)
/// 4. `Mix` → Master
///
/// 順序が肝: SidechainTap は Track 1 の scratch が埋まった **後** で
/// Track 2 の plugin が process() を呼ばれる **前** に挿入される。
/// engine 側はこの op を見て plugin の `pd.buffer_aux_in[0]` に
/// Track 1 の signal を copy してから plugin.process() を dispatch する。
/// v29: 宛先 plugin は安定 device id で焼き込まれる。r.md #129 T4: 宛先が内蔵 Comp でも同じ位置に
/// `NativeSidechainTap` が出て、その場で受け皿が `Staged` になる。
#[test]
fn sidechain_emits_tap_before_destination_process_track() {
    for kind in SC_CONSUMERS {
        sidechain_emits_tap_before_destination_process_track_for(kind);
    }
}

fn sidechain_emits_tap_before_destination_process_track_for(kind: ScConsumer) {
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Source".into();
            }),
            track(|t| {
                t.id = 2;
                t.name = "Dest".into();
                // aux input port 0 ← Track 1's output
                t.devices = vec![sc_consumer(kind, 77, 1)];
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

    // (a) tap が emit されている (安定 device id で addressing)。
    let tap_idx = sc_tap_position(&sched, kind, BufRef::TrackScratch(0), 77, 1).unwrap_or_else(|| {
        panic!("{kind:?}: expected tap (src=TrackScratch(0), device 77); nodes={:?}", sched.nodes)
    });

    // (b) source track の ProcessTrack が tap より前 (= source scratch
    //     が埋まってから tap で copy される)。
    let src_proc_idx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }))
        .expect("ProcessTrack(0) missing");
    assert!(
        src_proc_idx < tap_idx,
        "source ProcessTrack must run before SidechainTap: src={src_proc_idx} tap={tap_idx}"
    );

    // (c) destination track の ProcessTrack が tap より後 (= plugin
    //     process() が呼ばれる前に sidechain buffer が埋まる)。
    let dst_proc_idx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 1 }))
        .expect("ProcessTrack(1) missing");
    assert!(
        tap_idx < dst_proc_idx,
        "SidechainTap must run before destination ProcessTrack: tap={tap_idx} dst={dst_proc_idx}"
    );
}

#[test]
fn master_fx_sidechain_emits_tap_after_master_mix() {
    for kind in SC_CONSUMERS {
        master_fx_sidechain_emits_tap_after_master_mix_for(kind);
    }
}

fn master_fx_sidechain_emits_tap_after_master_mix_for(kind: ScConsumer) {
    // Track 1 → master bus fx[0] の aux input。 master fx の tap は
    // master Mix の **後** (source scratch 確定後) に emit される。
    let song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.name = "Source".into();
        })],
        master_fx_chain: vec![sc_consumer(kind, 900, 1)],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

    let tap_idx = sc_tap_position(&sched, kind, BufRef::TrackScratch(0), 900, MASTER_OWNER).unwrap_or_else(|| {
        panic!("{kind:?}: expected master tap (src=TrackScratch(0), device 900); nodes={:?}", sched.nodes)
    });

    // master Mix が tap より前 (= 全 track mix 後に source scratch から copy)。
    let master_mix_idx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, .. }))
        .expect("master Mix missing");
    assert!(
        master_mix_idx < tap_idx,
        "master SidechainTap must run after master Mix: mix={master_mix_idx} tap={tap_idx}"
    );
}

/// PR4 sidechain × PDC integration: dest track の plugin が source track
/// から sidechain 入力を受けている場合、 dest 自身の `path_latency` は
/// 「sidechain source の path_latency」 を **input bus latency として
/// 取り込んだ上で** dest 自身の chain latency を加算する。 こうすると
/// master mix の sibling alignment が source の遅延分も補償する。
///
/// 仕様根拠: Ardour `route.cc` の `Latent` 基底クラス + sidechain
/// (`Send`/`PluginInsert::sidechain_input`) の implicit synchronization。
/// 実装上は sidechain edge を `compute_path_latency` の input fan-in に
/// 加算するだけで graph layer の不変量が成立する。
///
/// セットアップ:
///   Track A (id=1, latency 100) → master, source for sidechain
///   Track B (id=2, latency 50, fx slot 0 sidechain ← A) → master
///
/// 修正前 (PDC が sidechain edge を見ていない):
///   path_latency(A) = 100, path_latency(B) = 50
///   master mix max = 100, B が 50 サンプル遅延される (B が low-latency)。
///   → B の plugin は main を「即時」、 aux を A の遅延済み信号で受ける。
///   master 上では B が遅延されて A と「揃う」 が、 plugin 内部の sidechain
///   検出は時間軸ずれの状態。 さらに B の output に master mix delay が
///   掛かるので musical alignment が壊れる方向に動く。
///
/// 修正後 (sidechain edge を path_latency に取り込む):
///   path_latency(B) = max(0, path_latency(A)=100) + 50 = 150
///   master mix max = 150, A が 50 サンプル遅延される (A が low-latency)。
///   → master 上の musical alignment が一致する。 sibling drift が消える。
///
/// 残課題 (本テスト範囲外): plugin の main vs aux 内部 alignment は per-slot
/// chain prefix latency が必要なので、 別 PR で `DelayTrackInput` op を
/// 入れて対応。
#[test]
fn pdc_sidechain_source_path_latency_propagates_to_dest() {
    for kind in SC_CONSUMERS {
        pdc_sidechain_source_path_latency_propagates_to_dest_for(kind);
    }
}

fn pdc_sidechain_source_path_latency_propagates_to_dest_for(kind: ScConsumer) {
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Source".into();
                t.devices = latency_chain(&mut lat, 10, 100);
            }),
            track(|t| {
                t.id = 2;
                t.name = "Dest".into();
                // consumer + 50 sample の device (内蔵 device は遅延 0 なので、chain の遅延は別 device で持つ)。
                t.devices = vec![sc_consumer(kind, 20, 1)];
                t.devices.extend(latency_chain(&mut lat, 25, 50));
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    // Master Mix の input は (TrackScratch(0), TrackScratch(1)) の 2 本。
    // path_latency(A=0) = 100, path_latency(B=1) = 100 + 50 = 150 になっている
    // はずなので、 max=150 に対し A (=100) を 50 サンプル遅延する `ApplyDelay`
    // が master mix の直前に出る。
    let master_mix_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::Master,
                    ..
                }
            )
        })
        .expect("Master Mix must exist");

    // Source (TrackScratch(0)) が低 latency 側、 50 サンプル補償される。
    let apply_pos = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::ApplyDelay {
                    buf: BufRef::TrackScratch(0),
                    frames: 50,
                    ..
                }
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "expected ApplyDelay {{ TrackScratch(0), frames: 50 }} \
                 to align Source(latency 100) with Dest(input 100 + chain 50 = 150) \
                 before Master Mix; got nodes={:?}",
                sched.nodes
            )
        });
    assert!(
        apply_pos < master_mix_pos,
        "compensating ApplyDelay must come before Master Mix"
    );

    // Dest (TrackScratch(1)) には ApplyDelay が刺さらない (= max-latency 側)。
    let dest_has_delay = sched.nodes.iter().any(|op| {
        matches!(
            op,
            NodeOp::ApplyDelay {
                buf: BufRef::TrackScratch(1),
                ..
            }
        )
    });
    assert!(
        !dest_has_delay,
        "the highest-latency path (Dest including sidechain input) must NOT receive ApplyDelay"
    );
}

/// PR4.5 plugin-internal alignment: dest track の fx_chain plugin が
/// sidechain 入力を持つとき、 `Schedule::input_delay_per_track` に dest
/// track の input_delay_samples として「sidechain source の max
/// path_latency」 が記録される。 これは engine 側で `process_track_owned`
/// が instrument 出力 → fx_chain の境目で delay を入れて plugin の
/// main vs aux を時刻揃えするための spec。
///
/// 本テストは graph layer のみを検証 (engine の delay 適用は別レイヤ)。
/// fx_chain の sidechain は `input_delay` に反映、 midi_fx_chain /
/// instrument の sidechain は反映しない (= MVP scope。 instrument の
/// sidechain alignment は MIDI event 側も遅延させる必要があり、 別 PR)。
#[test]
fn pdc_sidechain_input_delay_recorded_for_dest_fx_chain_track() {
    for kind in SC_CONSUMERS {
        pdc_sidechain_input_delay_recorded_for_dest_fx_chain_track_for(kind);
    }
}

fn pdc_sidechain_input_delay_recorded_for_dest_fx_chain_track_for(kind: ScConsumer) {
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Source".into();
                t.devices = latency_chain(&mut lat, 10, 100);
            }),
            track(|t| {
                t.id = 2;
                t.name = "Dest".into();
                t.devices = vec![sc_consumer(kind, 20, 1)];
                t.devices.extend(latency_chain(&mut lat, 25, 50));
            }),
            track(|t| {
                t.id = 3;
                t.name = "Bystander".into();
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    assert_eq!(
        sched.input_delay_per_track.len(),
        3,
        "input_delay_per_track must have one entry per track"
    );
    assert_eq!(
        sched.input_delay_per_track[0], 0,
        "Source has no sidechain inputs, so input_delay = 0"
    );
    assert_eq!(
        sched.input_delay_per_track[1], 100,
        "Dest receives sidechain from Source(path_latency=100), \
         so input_delay must equal Source.path_latency"
    );
    assert_eq!(
        sched.input_delay_per_track[2], 0,
        "Bystander has no sidechain wiring, input_delay = 0"
    );
}

/// §5 (arch refactor): **leaf** 宛の sidechain tap は staging (post-
/// dispatch) と消費 (次 buffer の pass-1 process) が 1 buffer ずれるので、
/// `buffer_frames` が入力遅延と path latency の両方に加算される。bus 宛
/// (return の ProcessGroupFx) は同 buffer 消費なので加算されず、track の入力遅延ではなく
/// `ProcessGroupFx` の直前の `BusScAlign` で揃える (r.md #129 §8.3.3)。ここでは return の入力
/// (send 元) が SC 元と同じ track なので、揃える量は 0 = 遅延を積まない。
#[test]
fn pdc_leaf_sidechain_tap_adds_one_buffer_of_lag() {
    for kind in SC_CONSUMERS {
        pdc_leaf_sidechain_tap_adds_one_buffer_of_lag_for(kind);
    }
}

fn pdc_leaf_sidechain_tap_adds_one_buffer_of_lag_for(kind: ScConsumer) {
    use common::model::{Send, SendMode};

    const BUF: u32 = 512;
    let sc_device = |id: u64| sc_consumer(kind, id, 1);
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Source".into();
                t.devices = latency_chain(&mut lat, 10, 100);
                // Bus (id 3) へ send → id 3 は return bus になる。
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 3,
                    gain: 1.0,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.name = "LeafDest".into();
                t.devices = vec![sc_device(20)];
            }),
            track(|t| {
                t.id = 3;
                t.name = "BusDest".into();
                t.devices = vec![sc_device(30)];
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, BUF, RenderScope::Mix).unwrap();

    // leaf 宛: source path latency (100) + 1 buffer (512)。
    assert_eq!(
        sched.input_delay_per_track[1],
        100 + BUF,
        "leaf-destined tap must include the 1-buffer staging lag"
    );
    // bus 宛 (return): pass 2 の consumer なので track の入力遅延は持たない。
    assert_eq!(sched.input_delay_per_track[2], 0, "pass-2 consumer は input delay を使わない");
    assert!(
        !sched.delay_keys.iter().any(|k| matches!(k, DelayKey::BusScAlign { .. })),
        "send 元 (= SC 元) で入力が既に 100 遅れているので BusScAlign は積まない: {:?}",
        sched.delay_keys
    );
}

/// PR4.5 plugin-internal alignment: midi_fx_chain や instrument に
/// sidechain wiring があっても `input_delay_per_track` には反映しない。
/// これは MVP scope 制限 (instrument input は MIDI 経由なので audio
/// stream に delay を入れるだけでは不十分、 MIDI event も遅延させる
/// 必要がある — 別 PR)。
#[test]
fn pdc_sidechain_instrument_input_delay_skipped_in_mvp() {
    use common::model::PluginInstance;
    use common::plugin_format::PluginFormat;

    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Source".into();
                t.devices = latency_chain(&mut lat, 10, 100);
            }),
            track(|t| {
                t.id = 2;
                t.name = "Dest".into();
                // v23 single-chain: an instrument device (note in + audio
                // out) → derives as Instrument, NOT AudioEffect, so its
                // sidechain does not contribute to input_delay_per_track.
                t.devices = vec![Device::Plugin(PluginInstance {
                    aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                    ..PluginInstance::with_ports(
                        "test.synth".into(),
                        PluginFormat::Vst3,
                        instrument_ports(),
                    )
                })];
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    // path_latency は instrument の sidechain も拾う (= 100 + 0 = 100)
    // ので master mix の sibling alignment は機能する。
    // ただし input_delay_per_track には乗らない (MVP scope)。
    assert_eq!(
        sched.input_delay_per_track[1], 0,
        "instrument sidechain は input_delay に反映しない (MVP scope)"
    );
}

/// PR4 sidechain × PDC: sidechain edge が cycle を作る (A→B→A) 場合、
/// `compile_schedule` は `GraphError::Cycle` を返す。 graph layer で
/// 検出しないと `compute_path_latency` が無限再帰する。
#[test]
fn sidechain_cycle_between_two_tracks_is_rejected() {
    for kind in SC_CONSUMERS {
        // A(id=1) の consumer が B(id=2) からの sidechain を、
        // B(id=2) の consumer が A(id=1) からの sidechain を要求 → cycle。
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "A".into();
                    t.devices = vec![sc_consumer(kind, 11, 2)];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "B".into();
                    t.devices = vec![sc_consumer(kind, 12, 1)];
                }),
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle), "{kind:?}");
    }
}

/// r.md #129 T4: native の consumer が「処理しうるか」は `can_activate` (静的に ON、または On のレーン /
/// 変調がある)。bypass 中で On レーンの無い Comp は依存辺にも tap にも数えない = 循環しない。On レーンが
/// あれば処理しうるので数える (レーンで ON になった瞬間から外部 SC で検出する)。
#[test]
fn a_bypassed_native_consumer_counts_only_when_it_can_activate() {
    use common::model::{AutomationLane, AutomationTarget, NativeKind, NativeParamId};
    let song_with = |on_lane: bool| {
        let mut b = sc_consumer(ScConsumer::Native, 12, 1);
        b.set_bypassed(true);
        Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.devices = vec![sc_consumer(ScConsumer::Native, 11, 2)];
                }),
                track(|t| {
                    t.id = 2;
                    t.devices = vec![b];
                    if on_lane {
                        let on = AutomationTarget::NativeParam { device_id: 12, param: NativeParamId::On(NativeKind::Comp) };
                        t.automation_lanes = vec![AutomationLane::new(on, 1.0)];
                    }
                }),
            ],
            ..Song::default()
        }
    };
    let sched = compile_schedule_for_test(&song_with(false), 48_000, 0).expect("bypass 中の配線は循環しない");
    assert!(sc_tap_position(&sched, ScConsumer::Native, BufRef::TrackScratch(0), 12, 1).is_none());
    assert!(sc_tap_position(&sched, ScConsumer::Native, BufRef::TrackScratch(1), 11, 0).is_some());
    assert_eq!(compile_schedule_for_test(&song_with(true), 48_000, 0).err(), Some(GraphError::Cycle));
}

/// 同じく compile-level test: `sidechain_sources` の対象 track が
/// 存在しない (DanglingReference) 場合は無視する (Tap を emit しない、
/// schedule 全体は壊さない)。 schedule 自体の compile error にすると
/// 編集中に他の compile error が track 間で連鎖して厄介なので、
/// 寛容に扱う。
#[test]
fn sidechain_with_dangling_source_track_is_skipped() {
    for kind in SC_CONSUMERS {
        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.name = "Lone".into();
                // 存在しない track を指す → dangling、 Tap は emit されない
                t.devices = vec![sc_consumer(kind, 11, 99)];
            })],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(!any_sc_tap(&sched), "{kind:?}: dangling sidechain source must not emit a tap; nodes={:?}", sched.nodes);
        let p = &sched.track_programs[0];
        if kind == ScConsumer::Native {
            assert_eq!(p.natives[0].sc_mode, crate::graph::native::ScMode::None, "解決できない配線は自分の入力で検出");
        }
    }
}

// ---- PR4 aux send / return ----

#[test]
fn send_emits_mixsend_into_return_bus_after_clear_before_group_fx() {
    use common::model::{Send, SendMode};

    // Vocal (id 1, idx 0) post-fader sends to Reverb (id 2, idx 1).
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Vocal".into();
                t.sends = vec![Send {
                    id: 11,
                    dest_track_id: 2,
                    gain: 0.5,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.name = "Reverb".into();
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

    // Vocal is a leaf → ProcessTrack(0). Reverb has an incoming send,
    // so it is a bus → Mix(clear) + MixSend + ProcessGroupFx(1).
    let vocal_proc = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }))
        .expect("Vocal ProcessTrack");
    let reverb_clear = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::TrackScratch(1),
                    ..
                }
            )
        })
        .expect("Reverb clearing Mix");
    let mixsend = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::MixSend {
                    src: BufRef::TrackScratch(0),
                    dst: BufRef::TrackScratch(1),
                    src_track_idx: 0,
                    send_id: 11,
                }
            )
        })
        .unwrap_or_else(|| panic!("expected MixSend Vocal→Reverb; nodes={:?}", sched.nodes));
    let reverb_fx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, .. }))
        .expect("Reverb ProcessGroupFx");

    assert!(
        vocal_proc < mixsend,
        "source must process before its send is mixed in"
    );
    assert!(
        reverb_clear < mixsend,
        "return scratch must be cleared before sends accumulate"
    );
    assert!(
        mixsend < reverb_fx,
        "sends must accumulate before the return's fx chain runs"
    );

    // Vocal still feeds master dry; Reverb feeds master wet.
    let master_idxs: Vec<u32> = sched
        .nodes
        .iter()
        .find_map(|op| match op {
            NodeOp::Mix {
                dst: BufRef::Master,
                srcs,
            } => Some(srcs.clone()),
            _ => None,
        })
        .unwrap()
        .iter()
        .map(|(b, _)| match b {
            BufRef::TrackScratch(i) => *i,
            other => panic!("unexpected master src {other:?}"),
        })
        .collect();
    assert!(master_idxs.contains(&0), "Vocal dry must still reach master");
    assert!(master_idxs.contains(&1), "Reverb return must reach master");
}

#[test]
fn pre_fader_send_taps_pre_fader_scratch() {
    use common::model::{Send, SendMode};

    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Vocal".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 2,
                    gain: 1.0,
                    mode: SendMode::PreFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.name = "Cue".into();
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
    assert!(
        sched.nodes.iter().any(|op| matches!(
            op,
            NodeOp::MixSend {
                src: BufRef::PreFaderScratch(0),
                dst: BufRef::TrackScratch(1),
                ..
            }
        )),
        "pre-fader send must tap the source's PreFaderScratch; nodes={:?}",
        sched.nodes
    );
}

#[test]
fn self_send_is_rejected_as_cycle() {
    use common::model::{Send, SendMode};

    let song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.name = "A".into();
            t.sends = vec![Send {
                id: 1,
                dest_track_id: 1, // sends to itself
                gain: 1.0,
                mode: SendMode::PostFader,
                enabled: true,
            }];
        })],
        ..Song::default()
    };
    assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
}

#[test]
fn send_loop_between_two_returns_is_rejected() {
    use common::model::{Send, SendMode};

    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "A".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 2,
                    gain: 1.0,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.name = "B".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 1,
                    gain: 1.0,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
        ],
        ..Song::default()
    };
    assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
}

#[test]
fn send_to_dangling_dest_is_skipped() {
    use common::model::{Send, SendMode};

    let song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.name = "Lone".into();
            t.sends = vec![Send {
                id: 1,
                dest_track_id: 99, // no such track
                gain: 1.0,
                mode: SendMode::PostFader,
                enabled: true,
            }];
        })],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
    assert!(
        !sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::MixSend { .. })),
        "dangling send dest must not emit MixSend; nodes={:?}",
        sched.nodes
    );
    // With no valid routing the track stays a plain leaf.
    assert!(
        sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 })),
        "Lone with only a dangling send must remain a ProcessTrack leaf"
    );
}

#[test]
fn send_source_latency_aligns_return_with_dry_at_master() {
    use common::model::{Send, SendMode};

    // Vocal (idx 0, latency 0) sends to Reverb (idx 1, reported
    // latency 100) and also feeds master dry. Reverb's path latency =
    // max(send src Vocal = 0) + 100 = 100, so at the master mix the
    // dry Vocal (latency 0) must be delayed 100 to align with the wet.
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Vocal".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 2,
                    gain: 0.5,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            }),
            track(|t| {
                t.id = 2;
                t.name = "Reverb".into();
                t.devices = latency_chain(&mut lat, 20, 100);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).unwrap();

    let master_mix = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::Mix {
                    dst: BufRef::Master,
                    ..
                }
            )
        })
        .expect("master Mix");
    let apply = sched
        .nodes
        .iter()
        .position(|op| {
            matches!(
                op,
                NodeOp::ApplyDelay {
                    buf: BufRef::TrackScratch(0),
                    frames: 100,
                    ..
                }
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "expected ApplyDelay {{ TrackScratch(0), frames: 100 }} before master; \
                 nodes={:?}",
                sched.nodes
            )
        });
    assert!(apply < master_mix, "dry-path delay must precede the master mix");

    // The MixSend taps Vocal's undelayed scratch before that delay is
    // applied (the send was already consumed by the Reverb bus).
    let mixsend = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::MixSend { .. }))
        .expect("MixSend");
    assert!(
        mixsend < apply,
        "the send must read the source before the master dry-delay mutates it"
    );
}

// ---- パラアウト (docs/plan_paraout.md) ----

/// パラアウト 全部子 (docs/plan_paraout.md): a multi-out instrument on track
/// A (id 1) routes EVERY output to a child — port 0 (MAIN) → 子2 (id 2),
/// aux bus 0 (port 1) → 子3 (id 3) — both parenting back to A. A becomes a
/// **pure bus** (keeps no own main); 子2/子3 are **paraout-dest buses**, no cycle:
///  - per port: a `ParallelOutTap` (port 0 = main, port 1.. = aux buses)
///  - A: a **clearing** `Mix` into its own scratch (no own main to keep) +
///    `ProcessGroupFx { start_device: split }` (suffix FX only)
///  - A must NOT emit `MixAdditive` (nothing of its own to preserve)
///  - children processed before A sums them
#[test]
fn paraout_all_children_clears_main_and_taps_every_port() {
    use common::model::{AuxOutputRoute, PluginInstance};
    use common::plugin_format::PluginFormat;

    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Drums".into();
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 10,
                    // port 0 (main) → 子2, port 1 (aux bus 0) → 子3 = 全部子
                    aux_outputs: vec![
                        Some(AuxOutputRoute::to_track(2)),
                        Some(AuxOutputRoute::to_track(3)),
                    ],
                    aux_output_count: 2,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        instrument_ports(),
                    )
                })];
            }),
            track(|t| {
                t.id = 2;
                t.name = "Kick".into();
                t.parent_group_id = Some(1);
            }),
            track(|t| {
                t.id = 3;
                t.name = "Snare".into();
                t.parent_group_id = Some(1);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).expect("全部子 must not cycle");

    // (a) ParallelOutTap per port: main(port0) → 子2(idx1), aux0(port1) → 子3(idx2).
    //     v29: source plugin は安定 device id (10) で焼き込まれる。
    assert!(
        sched.nodes.iter().any(|op| matches!(
            op,
            NodeOp::ParallelOutTap { device_id: 10, port: 0, dst_track: 1 }
        )),
        "expected ParallelOutTap main(port0) → 子2(idx1); nodes={:?}",
        sched.nodes
    );
    assert!(
        sched.nodes.iter().any(|op| matches!(
            op,
            NodeOp::ParallelOutTap { device_id: 10, port: 1, dst_track: 2 }
        )),
        "expected ParallelOutTap aux0(port1) → 子3(idx2)"
    );

    // (b) 全部子: A clears + sums ALL children via a clearing Mix (main went
    //     to 子2 via port 0), running suffix FX from the split (device 1).
    let sum_mix = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::TrackScratch(0), .. }))
        .expect("全部子 A must use a clearing Mix into its own scratch");
    assert!(
        sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 0, start_op: 1 })),
        "A's suffix FX must start at the split (device 1); nodes={:?}",
        sched.nodes
    );
    // (c) A keeps no own main, so it must NOT use MixAdditive.
    assert!(
        !sched.nodes.iter().any(|op| matches!(op, NodeOp::MixAdditive { .. })),
        "全部子 A must not emit MixAdditive (main went to 子2 via port 0)"
    );

    // (d) children's bus FX run before A sums them.
    let b_fx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, .. }))
        .expect("子2 ProcessGroupFx");
    let c_fx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 2, .. }))
        .expect("子3 ProcessGroupFx");
    assert!(
        b_fx < sum_mix && c_fx < sum_mix,
        "children must be processed before A's clearing Mix sums them"
    );

    // (e) A's clearing Mix carries both children.
    let srcs = sched
        .nodes
        .iter()
        .find_map(|op| match op {
            NodeOp::Mix { dst: BufRef::TrackScratch(0), srcs } => Some(srcs.clone()),
            _ => None,
        })
        .unwrap();
    let idxs: Vec<u32> = srcs
        .iter()
        .map(|(b, _)| match b {
            BufRef::TrackScratch(i) => *i,
            o => panic!("unexpected Mix src {o:?}"),
        })
        .collect();
    assert!(
        idxs.contains(&1) && idxs.contains(&2),
        "A must sum children 子2(1) and 子3(2); got {idxs:?}"
    );
}

/// Independent-topology paraout: A (id 1) routes an aux output to D (id 4)
/// which is NOT a child of A (D → master directly). A stays a plain leaf
/// (`ProcessTrack`, full chain), D becomes a paraout-dest bus, and the tap
/// flows A.aux → D. No cycle, no MixAdditive (A has no children).
#[test]
fn paraout_independent_dest_keeps_source_a_leaf() {
    use common::model::{AuxOutputRoute, PluginInstance};
    use common::plugin_format::PluginFormat;

    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Drums".into();
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 10,
                    aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                    aux_output_count: 1,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        instrument_ports(),
                    )
                })];
            }),
            track(|t| {
                t.id = 4;
                t.name = "Snare".into();
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 0).expect("independent paraout must not cycle");

    // A (idx 0) is a plain leaf: ProcessTrack, no MixAdditive.
    assert!(
        sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 })),
        "source A must remain a leaf (ProcessTrack); nodes={:?}",
        sched.nodes
    );
    assert!(
        !sched.nodes.iter().any(|op| matches!(op, NodeOp::MixAdditive { .. })),
        "no MixAdditive when the source has no children"
    );
    // D (idx 1) is a paraout-dest bus receiving A's aux (device_id 10).
    assert!(
        sched.nodes.iter().any(|op| matches!(
            op,
            NodeOp::ParallelOutTap { device_id: 10, port: 0, dst_track: 1 }
        )),
        "expected ParallelOutTap A.dev(10).port0 → D(idx1); nodes={:?}",
        sched.nodes
    );
    assert!(
        sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, start_op: 0 })),
        "D must run as a bus (ProcessGroupFx start_op 0)"
    );
}

/// パラアウト PDC 楽器兼バス (docs/plan_paraout.md): main を親に残す
/// (port 0 unrouted) group-with-instrument で子に latency 持ちプラグインが
/// あると、 MixAdditive の直前に「A の main (dst scratch、 prefix 後で相対 0)」
/// と latency の小さい子を最大 path latency に揃える `ApplyDelay` が入る。
/// これが無いとキック (main) とスネア (子経由) がサンプルずれる。 emit を検証
/// (ApplyDelay handler の数値正しさは既存の
/// `pdc_two_track_impulse_aligns_at_master_with_loaded_latency_plugin` が担保)。
#[test]
fn paraout_instrument_bus_pdc_aligns_main_and_children() {
    use common::model::{AuxOutputRoute, PluginInstance};
    use common::plugin_format::PluginFormat;

    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Drums".into();
                t.devices = vec![Device::Plugin(PluginInstance {
                    // port 0 (main) unrouted = 楽器兼バス (the kick stays on A);
                    // aux bus 0/1 (port 1/2) → 子2/子3.
                    aux_outputs: vec![
                        None,
                        Some(AuxOutputRoute::to_track(2)),
                        Some(AuxOutputRoute::to_track(3)),
                    ],
                    aux_output_count: 3,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        instrument_ports(),
                    )
                })];
            }),
            track(|t| {
                t.id = 2;
                t.name = "Snare".into();
                t.parent_group_id = Some(1);
                t.devices = latency_chain(&mut lat, 20, 100); // 子に latency 持ち FX
            }),
            track(|t| {
                t.id = 3;
                t.name = "HiHat".into();
                t.parent_group_id = Some(1);
                // latency なし
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).expect("must compile");

    let add_mix = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::MixAdditive { dst: BufRef::TrackScratch(0), .. }))
        .expect("A MixAdditive");

    // A の main (idx0) を子の max latency (100) に揃える ApplyDelay が
    // MixAdditive の直前に在る。
    assert!(
        sched.nodes[..add_mix].iter().any(|op| matches!(
            op,
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(0), frames: 100, .. }
        )),
        "A's instrument main must be delayed 100 to align with the latent child; nodes={:?}",
        sched.nodes
    );
    // latency の無い子 HiHat (idx2) も 100 揃え。
    assert!(
        sched.nodes[..add_mix].iter().any(|op| matches!(
            op,
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(2), frames: 100, .. }
        )),
        "HiHat (no latency) must be delayed 100 to align with Snare; nodes={:?}",
        sched.nodes
    );
    // latency 持ちの Snare (idx1) は max なので揃え不要 (ApplyDelay 無し)。
    assert!(
        !sched.nodes[..add_mix].iter().any(|op| matches!(
            op,
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(1), .. }
        )),
        "Snare (the max-latency child) needs no compensating delay"
    );
}

/// パラアウト独立 dest の PDC fan-in (docs/plan_paraout.md): source A の aux を
/// 子でない D へ振ると、 D の path latency に A の latency が乗り、 master で
/// 他トラックと揃う。 fan-in が無いと D が二重遅延 (= A.aux で既に遅れている
/// のに更に PDC で遅らされる) になる。
#[test]
fn paraout_independent_dest_pdc_fans_in_source_latency() {
    use common::model::{AuxOutputRoute, PluginInstance};
    use common::plugin_format::PluginFormat;

    let mut lat = DeviceLatencies::new();
    lat.insert(10, 100); // source に latency
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.name = "Drums".into();
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 10,
                    aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                    aux_output_count: 1,
                    ..PluginInstance::with_ports(
                        "test.drum_sampler".into(),
                        PluginFormat::Clap,
                        instrument_ports(),
                    )
                })];
            }),
            track(|t| {
                t.id = 4;
                t.name = "Snare".into(); // 独立 dest (A の子でない)、 latency なし
            }),
            track(|t| {
                t.id = 5;
                t.name = "Dry".into(); // 整合相手、 latency なし
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, 0, RenderScope::Mix).expect("must compile");

    let master_mix = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, .. }))
        .expect("master Mix");

    // D (idx1) は A.aux (100 遅れ) を受けるので path latency 100。 master で
    // 余計な ApplyDelay は入らない (既に max)。
    assert!(
        !sched.nodes[..master_mix].iter().any(|op| matches!(
            op,
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(1), .. }
        )),
        "independent dest D already carries source latency; must not be delayed again; nodes={:?}",
        sched.nodes
    );
    // Dry (idx2, latency 0) は master で 100 揃え (A/D が 100 で max)。
    assert!(
        sched.nodes[..master_mix].iter().any(|op| matches!(
            op,
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(2), frames: 100, .. }
        )),
        "the dry track must be delayed 100 to align with the paraout chain (A+D); nodes={:?}",
        sched.nodes
    );
}

// ---- r.md #110 Parallel ----------------------------------------------------

fn parallel_with_chains(id: u64, chains: Vec<(u64, Vec<Device>)>) -> Device {
    Device::Parallel(common::model::Parallel {
        id,
        name: "Parallel".into(),
        chains: chains
            .into_iter()
            .map(|(cid, devices)| common::model::ParallelChain {
                id: cid,
                devices,
                ..common::model::ParallelChain::new("c")
            })
            .collect(),
        bypassed: false,
        color: None,
        out_gain: 1.0,
        gain_match: false,
        split: common::model::Split::None,
    })
}

fn comp_tapping(id: u64, tap: common::model::AudioTap) -> Device {
    Device::Plugin(PluginInstance {
        id,
        aux_inputs: vec![Some(common::model::AuxInputRoute { tap })],
        ..PluginInstance::with_ports("test.compressor".into(), PluginFormat::Vst3, audio_fx_ports())
    })
}

/// 自 track の Pre-FX を SC の key にする: 依存辺も `SidechainTap` も出さず、 program の
/// `Plugin` op に port bit が立つ (同 pass の snapshot を直接載せる)。
#[test]
fn sidechain_from_own_track_prefx_is_staged_in_program_not_schedule() {
    let mut song = Song::default();
    let mut comp = PluginInstance::new("comp".into(), PluginFormat::Clap);
    comp.id = 77;
    comp.ports = PortConfig { has_audio_input: true, has_audio_output: true, ..PortConfig::default() };
    comp.aux_inputs = vec![Some(common::model::AuxInputRoute {
        tap: AudioTap::new(TapSource::Track(1), TapPoint::PreFx),
    })];
    song.tracks.push(Track { id: 1, devices: vec![Device::Plugin(comp)], ..Track::default() });
    let schedule = compile_schedule(&song, &DeviceLatencies::new(), 48_000, 256, RenderScope::Mix).expect("no cycle");
    assert!(!schedule.nodes.iter().any(|op| matches!(op, NodeOp::SidechainTap { .. })));
    assert!(schedule.track_programs[0].ops.iter().any(|op| matches!(
        op,
        super::super::program::ChainOp::Plugin { device_id: 77, own_prefx_ports: 0b1, .. }
    )));
    assert_eq!(schedule.input_delay_per_track[0], 0, "自 track の key は main と同じ信号なので遅延不要");
}

#[test]
fn sidechain_from_a_sibling_chain_taps_the_chain_snapshot() {
    use common::model::{AudioTap, TapPoint, TapSource};
    // track 1: Parallel { Dry (空, id 11), Comp (id 12: comp 77 が Dry chain の PostFx を key に) }
    let song = Song {
        tracks: vec![track(|t| {
            t.id = 1;
            t.devices = vec![parallel_with_chains(
                10,
                vec![
                    (11, vec![]),
                    (12, vec![comp_tapping(77, AudioTap::new(TapSource::Chain(11), TapPoint::PostFx))]),
                ],
            )];
        })],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 256).unwrap();
    // (a) tap は chain 11 の PostFx snapshot (owner = track 0, chain slot 0) を指す。
    assert!(
        sched.nodes.iter().any(|op| matches!(
            op,
            NodeOp::SidechainTap { src: BufRef::ChainPostFx { owner: 0, slot: 0 }, device_id: 77, aux_in_port: 0 }
        )),
        "chain tap: nodes={:?}",
        sched.nodes
    );
    // (b) program 側: chain 11 の ChainEnd に PostFx snapshot flag が焼かれている。
    let prog = &sched.track_programs[0];
    assert!(prog.ops.iter().any(|op| matches!(
        op,
        crate::graph::ChainOp::ChainEnd { chain_id: 11, snapshot_post_fx: true, .. }
    )));
    // (c) 同 track の chain source は自己辺にならず (cycle ではない)、main の入力
    //     alignment にも数えない。
    assert_eq!(sched.input_delay_per_track[0], 0);
}

#[test]
fn sidechain_from_another_tracks_chain_orders_the_owner_first() {
    use common::model::{AudioTap, TapPoint, TapSource};
    // track 2 の comp が track 1 の Parallel chain 11 (PostFader) を key にする。
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 2;
                t.devices = vec![comp_tapping(77, AudioTap::new(TapSource::Chain(11), TapPoint::PostFader))];
            }),
            track(|t| {
                t.id = 1;
                t.devices = vec![parallel_with_chains(10, vec![(11, vec![])])];
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule_for_test(&song, 48_000, 256).unwrap();
    let tap_idx = sched
        .nodes
        .iter()
        .position(|op| matches!(
            op,
            NodeOp::SidechainTap { src: BufRef::ChainPostFader { owner: 1, slot: 0 }, device_id: 77, .. }
        ))
        .unwrap_or_else(|| panic!("chain tap missing: {:?}", sched.nodes));
    let owner_idx = sched
        .nodes
        .iter()
        .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 1 }))
        .expect("owner ProcessTrack");
    assert!(owner_idx < tap_idx, "chain の所有 track が先に走る");
    // leaf 宛の tap は 1 buffer 遅れの補償が入る (track source と同じ規則)。
    assert_eq!(sched.input_delay_per_track[0], 256);
}

// ---- r.md #129: 内蔵 device (§15.3 T13 / T14 / T15) ----

fn op_labels(p: &crate::graph::ChainProgram) -> Vec<String> {
    use crate::graph::ChainOp;
    p.ops
        .iter()
        .map(|op| match op {
            ChainOp::Plugin { device_id, .. } => format!("P{device_id}"),
            ChainOp::Native { native_slot, .. } => {
                format!("N({:?})", p.natives[*native_slot as usize].dsp.kind())
            }
            ChainOp::ParallelBegin { .. } => "RB".into(),
            ChainOp::ChainBegin { .. } => "CB".into(),
            ChainOp::ChainEnd { .. } => "CE".into(),
            ChainOp::ParallelEnd { .. } => "RE".into(),
        })
        .collect()
}

/// pass 2 を 1 buffer 走らせる (pass 1 の出力は呼び側が scratch に置く)。
fn run_post_dispatch(
    sched: &mut Schedule,
    scratch: &mut [crate::mixer::TrackScratch],
    song: &Song,
    n: usize,
    io: crate::graph::NativeIo<'_>,
) {
    let refs: crate::engine::PluginRefs = std::collections::HashMap::new();
    let (mut ml, mut mr) = (vec![0.0; n], vec![0.0; n]);
    crate::graph::execute_schedule_post_dispatch(
        sched,
        scratch,
        &mut ml,
        &mut mr,
        n,
        song,
        &refs,
        48_000,
        n as u32,
        true,
        false,
        &std::collections::HashSet::new(),
        120.0,
        0.0,
        common::model::LoopRegion::default(),
        common::mod_plane::ModTickPlaneRef::default(),
        crate::mod_tick::FollowerDrive::default(),
        &crate::launcher::RowSourceTable::default(),
        io,
    );
}

fn native_sc(kind: common::model::NativeKind, id: u64, source: u32, point: common::model::TapPoint) -> Device {
    let mut d = common::model::NativeDevice::new_added(kind, id, 1);
    d.aux_input = Some(common::model::AuxInputRoute {
        tap: common::model::AudioTap::new(common::model::TapSource::Track(source), point),
    });
    Device::Native(d)
}

/// T13: v38 の strip を持つ旧形式の fixture を読み込むと、組み込みが旧 strip と同じ位置の op になる。
/// 通常トラックは `[P…, N(comp), N(eq)]`、GWI の組み込みは `pass1_end` より後 (= 旧 strip と同じ pass 2)、
/// master は `[N(bus), N(tone), P…]`。Limiter の遅延を焼くかは旧 `limiter.on` と一致する。
#[test]
fn v38_fixture_compiles_builtins_where_the_old_strip_ran() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../common/tests/fixtures/v38_strips.daw");
    let song = common::project::load_project(&path).expect("v38 fixture").song;
    let sched = compile_schedule_for_test(&song, 48_000, 256).expect("compile");
    // program の内蔵 device が song の **組み込み** device そのもの (device id で突き合わせる)。
    let builtin_ids = |devices: &[common::model::Device]| -> Vec<u64> {
        devices.iter().filter_map(|d| d.as_native().filter(|n| n.builtin).map(|n| n.id)).collect()
    };
    let native_ids = |p: &crate::graph::ChainProgram| -> Vec<u64> { p.natives.iter().map(|ns| ns.device_id).collect() };
    for (i, t) in song.tracks.iter().enumerate() {
        let p = &sched.track_programs[i];
        let labels = op_labels(p);
        assert_eq!(p.natives.len(), 2, "track {}: {labels:?}", t.name);
        assert_eq!(labels[labels.len() - 2..], ["N(Comp)", "N(Eq)"], "track {} の末尾: {labels:?}", t.name);
        assert_eq!(native_ids(p), builtin_ids(&t.devices), "track {} の内蔵 device は組み込み", t.name);
    }
    let gwi = song.tracks.iter().position(|t| t.name == "GWI").expect("GWI track");
    let p = &sched.track_programs[gwi];
    assert!(matches!(p.pass1_role, crate::graph::program::Pass1Role::GroupWithInstrument { .. }));
    let first_native = p.ops.iter().position(|op| matches!(op, crate::graph::ChainOp::Native { .. })).unwrap();
    assert!(p.pass1_end <= first_native, "GWI の組み込みは pass 2: pass1_end={} ops={:?}", p.pass1_end, op_labels(p));
    let master = op_labels(&sched.master_program);
    assert_eq!(master[..2], ["N(BusComp)", "N(ToneEq)"], "master の先頭: {master:?}");
    assert_eq!(native_ids(&sched.master_program), builtin_ids(&song.master_fx_chain), "master の内蔵 device は組み込み");
    assert!(master.len() > 2 && master[2].starts_with('P'), "組み込みの後ろに旧 fx chain: {master:?}");
    assert!(song.master_limiter.on, "fixture の旧 master strip は limiter ON");
    assert!(sched.master_limiter_latency);
    assert_eq!(sched.master_latency_samples, common::model::limiter_lookahead_samples(48_000));
}

/// T14: return R の Comp が latency L の track S を SC に読み、R への send 元 A は latency 0。
/// R の consumer は pass 2 なので、track の入力遅延ではなく `ProcessGroupFx(R)` の直前の
/// `ApplyDelay(BusScAlign)` で入力を L 遅らせ、master 合流では A に L の補償が入る。実行すると S と A に
/// 同じ曲位置のインパルスを入れて、R の検出信号 (Listen で聴く) と R の main が同じサンプルに揃う。
#[test]
fn a_return_native_comp_aligns_its_bus_input_to_the_sidechain_with_bus_sc_align() {
    use common::model::{NativeKind, Send, SendMode, TapPoint};
    const L: u32 = 100;
    let mut lat = DeviceLatencies::new();
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = latency_chain(&mut lat, 10, L);
            }),
            track(|t| {
                t.id = 2;
                t.sends = vec![Send { id: 1, dest_track_id: 4, gain: 1.0, mode: SendMode::PostFader, enabled: true }];
            }),
            track(|t| {
                t.id = 4;
                t.devices = vec![native_sc(NativeKind::Comp, 40, 1, TapPoint::PostFader)];
            }),
        ],
        ..Song::default()
    };
    let n = 256usize;
    let render = |listen: bool| {
        let mut sched = compile_schedule(&song, &lat, 48_000, n as u32, RenderScope::Mix).unwrap();
        let fx = sched.nodes.iter().position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 2, .. })).unwrap();
        assert!(
            matches!(sched.nodes[fx - 1], NodeOp::ApplyDelay { buf: BufRef::TrackScratch(2), frames: L, .. }),
            "ProcessGroupFx(R) の直前に L の BusScAlign: {:?}",
            sched.nodes
        );
        assert!(sched.delay_keys.contains(&DelayKey::BusScAlign { track_id: 4 }));
        let master = sched.nodes.iter().position(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, .. })).unwrap();
        assert!(sched.nodes[..master]
            .iter()
            .any(|op| matches!(op, NodeOp::ApplyDelay { buf: BufRef::TrackScratch(1), frames: L, .. })));
        assert_eq!(sched.input_delay_per_track[2], 0, "pass 2 の consumer は input delay を使わない");
        // pass 1 の結果を置く: S は自分の latency ぶん遅れて出る、A は遅れない。
        let mut scratch: Vec<crate::mixer::TrackScratch> = (0..3).map(|_| crate::mixer::TrackScratch::new()).collect();
        scratch[0].track_l[L as usize] = 1.0;
        scratch[0].track_r[L as usize] = 1.0;
        scratch[1].track_l[0] = 1.0;
        scratch[1].track_r[0] = 1.0;
        let io = crate::graph::NativeIo { sc_listen: if listen { 40 } else { 0 }, scopes: None };
        run_post_dispatch(&mut sched, &mut scratch, &song, n, io);
        let out = &scratch[2].track_l[..n];
        (0..n).max_by(|&a, &b| out[a].abs().total_cmp(&out[b].abs())).unwrap()
    };
    assert_eq!(render(true), L as usize, "R の検出信号 (S) は L に居る");
    assert_eq!(render(false), L as usize, "R の main (A の send) も L に揃う");
}

/// T14 (続き): group-with-instrument の prefix (pass 1) に居る consumer は次の buffer で消費するので
/// `input_delay = L + buffer_frames` (以前は bus 扱いで lag を 0 と数えていた、§18-L)。suffix (pass 2) の
/// consumer が居なければ `BusScAlign` は積まない。
#[test]
fn a_group_with_instrument_prefix_consumer_takes_the_pass_one_lag() {
    use common::model::{AuxOutputRoute, NativeKind, TapPoint};
    const L: u32 = 100;
    const BUF: u32 = 256;
    let mut lat = DeviceLatencies::new();
    let drum = Device::Plugin(PluginInstance {
        id: 7,
        aux_outputs: vec![None, Some(AuxOutputRoute { dest_track: 3 })],
        ..PluginInstance::with_ports("test.drum".into(), PluginFormat::Clap, instrument_ports())
    });
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = latency_chain(&mut lat, 10, L);
            }),
            track(|t| {
                t.id = 2;
                t.devices = vec![native_sc(NativeKind::Comp, 40, 1, TapPoint::PostFader), drum];
            }),
            track(|t| {
                t.id = 3;
                t.parent_group_id = Some(2);
            }),
        ],
        ..Song::default()
    };
    let sched = compile_schedule(&song, &lat, 48_000, BUF, RenderScope::Mix).unwrap();
    assert!(matches!(sched.track_programs[1].pass1_role, crate::graph::program::Pass1Role::GroupWithInstrument { .. }));
    assert_eq!(sched.input_delay_per_track[1], L + BUF);
    assert!(!sched.delay_keys.iter().any(|k| matches!(k, DelayKey::BusScAlign { .. })), "{:?}", sched.delay_keys);
}

/// T15: pre-fader send の無い group でも、PostFx を source にした SC は「今の buffer のチェーン後の信号」を
/// 読む (group の pre-fader snapshot の条件が leaf と同じになった、§18-C)。
#[test]
fn a_post_fx_tap_on_a_group_reads_this_buffers_chain_output() {
    use common::model::{EqBand, EqParam, NativeDevice, NativeKind, NativeParamId, TapPoint};
    let mut eq = NativeDevice::new_added(NativeKind::Eq, 11, 1);
    eq.set_param(NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain }, 9.0);
    let song = Song {
        tracks: vec![
            track(|t| {
                t.id = 2;
                t.devices = vec![Device::Native(eq)];
            }),
            track(|t| {
                t.id = 3;
                t.parent_group_id = Some(2);
            }),
            track(|t| {
                t.id = 1;
                t.devices = vec![native_sc(NativeKind::Comp, 40, 2, TapPoint::PostFx)];
            }),
        ],
        ..Song::default()
    };
    let n = 128usize;
    let mut sched = compile_schedule_for_test(&song, 48_000, n as u32).unwrap();
    assert!(sched.track_programs[0].snapshot_post_fx, "PostFx の tap があれば group も snapshot を取る");
    assert!(!sched.track_programs[1].snapshot_post_fx);
    let x: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() * 0.5).collect();
    let mut scratch: Vec<crate::mixer::TrackScratch> = (0..3).map(|_| crate::mixer::TrackScratch::new()).collect();
    scratch[1].track_l[..n].copy_from_slice(&x);
    scratch[1].track_r[..n].copy_from_slice(&x);
    run_post_dispatch(&mut sched, &mut scratch, &song, n, crate::graph::NativeIo::default());

    // 期待値 = 同じ EQ を単独で通した音 (新品の状態から)。
    let alone = sched_free_eq_output(&song, &x);
    let p = &sched.track_programs[2];
    let sc = p.natives[0].sc.as_ref().expect("Staged");
    assert_ne!(alone, x, "EQ が効いている");
    assert_eq!(&scratch[0].pre_fader_l[..n], &alone[..], "group の PostFx snapshot = この buffer のチェーン出力");
    assert_eq!(sc.signal(n).0, &alone[..], "leaf の SC は同じ buffer の group のチェーン出力を staging する");
}

/// `song.tracks[0]` の device 列を単独の program で通した L チャンネル。
fn sched_free_eq_output(song: &Song, x: &[f32]) -> Vec<f32> {
    let devices = &song.tracks[0].devices;
    let mut alone =
        crate::graph::build_program(devices, 2, None, &DeviceLatencies::new(), &Default::default(), RenderScope::Mix)
            .program;
    let (mut l, mut r) = (x.to_vec(), x.to_vec());
    let refs: crate::engine::PluginRefs = std::collections::HashMap::new();
    let rec = std::collections::HashSet::new();
    let ctx = crate::graph::ProgramCtx {
        song: Some(song),
        plugin_refs: &refs,
        worker_sync: None,
        sample_rate: 48_000,
        frames: x.len() as u32,
        playing: true,
        current_bpm: 120.0,
        playhead_beats: 0.0,
        loop_region: common::model::LoopRegion::default(),
        recording_lanes: &rec,
        mod_plane: common::mod_plane::ModTickPlaneRef::default(),
        rows: crate::launcher::TrackRows::default(),
        own_pre_fx: None,
        native: crate::graph::NativeIo::default(),
        index: &common::song_index::SongIndex::build(song),
        owner: common::model::ParamStoreAt::Track(0),
    };
    let (mut a, mut b) = (Vec::with_capacity(8), Vec::with_capacity(8));
    let len = alone.ops.len();
    crate::graph::run_chain_program(&mut alone, 0..len, &mut l, &mut r, &mut a, &mut b, &ctx);
    l
}

// ---- r.md #131: 無効トラック (`docs/plan_rmd_131_track_disable.md`) ----

/// op が読み書きする track index (scratch / program / send の送り元)。
fn op_tracks(op: &NodeOp) -> Vec<u32> {
    let buf = |b: &BufRef| match *b {
        BufRef::TrackScratch(i) | BufRef::PreFaderScratch(i) | BufRef::PreFxScratch(i) => Some(i),
        _ => None,
    };
    match op {
        NodeOp::ProcessTrack { track_idx } | NodeOp::ProcessGroupFx { track_idx, .. } => vec![*track_idx],
        NodeOp::Mix { srcs, dst } | NodeOp::MixAdditive { srcs, dst } => {
            srcs.iter().filter_map(|(b, _)| buf(b)).chain(buf(dst)).collect()
        }
        NodeOp::MixSend { src, dst, src_track_idx, .. } => buf(src).into_iter().chain(buf(dst)).chain([*src_track_idx]).collect(),
        NodeOp::ApplyDelay { buf: b, .. } => buf(b).into_iter().collect(),
        NodeOp::SidechainTap { src, .. } | NodeOp::EnvelopeFollow { src, .. } => buf(src).into_iter().collect(),
        NodeOp::NativeSidechainTap { src, owner, .. } => buf(src).into_iter().chain([*owner]).collect(),
        NodeOp::ParallelOutTap { dst_track, .. } => vec![*dst_track],
    }
}

/// 無効トラック (と無効 group の子孫) は **グラフに居ない**: 直列トレースの手も op も無く、program は空
/// (plugin の依頼も latency も無い)、master への合流にも出ない。戻すと元の schedule に戻る。
#[test]
fn 無効トラックと無効_group_の子孫はグラフに居ない() {
    use crate::graph::program::Pass1Role;
    use crate::graph::render_graph::Step;

    let mut lat = DeviceLatencies::new();
    let mut song = Song {
        tracks: vec![
            track(|t| {
                t.id = 1;
                t.devices = latency_chain(&mut lat, 10, 500);
            }),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
                t.devices = latency_chain(&mut lat, 11, 300);
            }),
            track(|t| {
                t.id = 3;
                t.devices = latency_chain(&mut lat, 12, 40);
            }),
        ],
        ..Song::default()
    };
    let live = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert_eq!(live.master_latency_samples, 800, "対照: 全部有効なら group の経路 300 + 500");

    song.set_tracks_enabled(&[1], false);
    let sched = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert_eq!(sched.graph.trace.iter().filter(|s| matches!(s, Step::Process(_))).collect::<Vec<_>>(), vec![&Step::Process(2)]);
    for (i, p) in sched.track_programs.iter().enumerate() {
        let off = i < 2;
        assert_eq!(p.pass1_role == Pass1Role::Disabled, off, "track idx {i}: {:?}", p.pass1_role);
        assert_eq!(p.ops.is_empty(), off, "無効トラックの program は device を展開しない (idx {i})");
    }
    let touched: Vec<u32> = sched.nodes.iter().flat_map(op_tracks).collect();
    assert!(touched.iter().all(|&i| i == 2), "無効トラックに触れる op がある: {:?}", sched.nodes);
    assert!(sched.nodes.iter().any(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, srcs } if srcs == &vec![(BufRef::TrackScratch(2), 1.0)])));
    assert_eq!(sched.master_latency_samples, 40, "無効な経路の latency は数えない");

    song.set_tracks_enabled(&[1], true);
    let back = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert_eq!(format!("{:?}", back.nodes), format!("{:?}", live.nodes), "戻すと同じ schedule");
    assert_eq!(back.master_latency_samples, 800);
}

/// 無効トラックを読む側 / 無効トラックへ送る側は **無音 / 変調なし**: send (無効な送り元 / 無効な return)、
/// サイドチェイン元、パラアウトの送り先、AudioTap (follower) 元、無効トラックに帰属する follower。
/// 役割は構造で決まる — 無効な送り元しか持たない return も bus のまま (自分のクリップを鳴らし始めない)。
#[test]
fn 無効トラックへの送りとそこを読む_tap_は出ない() {
    use crate::graph::program::Pass1Role;
    use common::model::{AudioTap, AuxInputRoute, AuxOutputRoute, FollowerConfig, ModSource, ModSourceKind, Send, SendMode};

    let send = |id: u32, dest: u32| Send { id, dest_track_id: dest, gain: 1.0, mode: SendMode::PreFader, enabled: true };
    let follower = |id: u32, owner: u32, tap: u32| ModSource {
        id,
        owner_track_id: owner,
        color: [0.0; 3],
        kind: ModSourceKind::EnvelopeFollower { tap: Some(AudioTap::post_fader(tap)), follower: FollowerConfig::default() },
        enabled: true,
    };
    let mut song = Song {
        tracks: vec![
            // idx 0: 無効にする送り元 (return 3 へ send)。
            track(|t| {
                t.id = 1;
                t.sends = vec![send(1, 3)];
            }),
            // idx 1: 有効。track 1 をサイドチェインに読み、無効にする return 4 へ send。
            track(|t| {
                t.id = 2;
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 77,
                    aux_inputs: vec![Some(AuxInputRoute::post_fader(1))],
                    ..PluginInstance::with_ports("test.compressor".into(), PluginFormat::Vst3, audio_fx_ports())
                })];
                t.sends = vec![send(2, 4)];
            }),
            track(|t| t.id = 3),
            track(|t| t.id = 4),
            // idx 4: パラアウト元 (有効) → 無効にする独立 dest 6。
            track(|t| {
                t.id = 5;
                t.devices = vec![Device::Plugin(PluginInstance {
                    id: 10,
                    aux_outputs: vec![Some(AuxOutputRoute::to_track(6))],
                    aux_output_count: 1,
                    ..PluginInstance::with_ports("test.drum_sampler".into(), PluginFormat::Clap, instrument_ports())
                })];
            }),
            track(|t| t.id = 6),
        ],
        // 21: 有効な track 2 に帰属し、無効にする track 1 を読む / 22: 無効にする track 1 に帰属し、有効な track 2 を読む。
        mod_sources: vec![follower(21, 2, 1), follower(22, 1, 2)],
        ..Song::default()
    };
    let lat = DeviceLatencies::new();
    let is_route = |op: &NodeOp| {
        matches!(op, NodeOp::MixSend { .. } | NodeOp::SidechainTap { .. } | NodeOp::ParallelOutTap { .. } | NodeOp::EnvelopeFollow { .. })
    };
    let live = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert_eq!(live.nodes.iter().filter(|op| is_route(op)).count(), 6, "対照: send 2 / SC 1 / パラアウト 1 / follower 2");
    assert_eq!(live.follower_keys, vec![21, 22]);

    song.set_tracks_enabled(&[1, 4, 6], false);
    let sched = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert!(!sched.nodes.iter().any(is_route), "無効トラックに触れる経路が残っている: {:?}", sched.nodes);
    assert_eq!(sched.follower_keys, vec![0, 0], "進まない follower は状態を引き継がない (固まった変調を残さない)");
    let roles: Vec<Pass1Role> = sched.track_programs.iter().map(|p| p.pass1_role).collect();
    assert_eq!(
        roles,
        vec![Pass1Role::Disabled, Pass1Role::Leaf, Pass1Role::Bus, Pass1Role::Disabled, Pass1Role::Leaf, Pass1Role::Disabled],
        "return 3 は無効な送り元しか持たなくても bus のまま"
    );
    assert_eq!(sched.input_delay_per_track, vec![0; 6], "無効な SC 元に揃える遅延は掛けない");
}

/// 無効トラックの solo は数えない (他を黙らせない)。solo の透過も無効な子からは流れ込まない。
#[test]
fn 無効トラックの_solo_は数えない() {
    let mut song = Song {
        tracks: vec![
            track(|t| t.id = 1),
            track(|t| {
                t.id = 2;
                t.parent_group_id = Some(1);
                t.solo = true;
            }),
            track(|t| t.id = 3),
        ],
        ..Song::default()
    };
    let lat = DeviceLatencies::new();
    let mut sched = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert!(sched.solo.any_solo(&song));
    sched.solo.resolve(&song);
    assert!(sched.solo.of(0).0, "対照: solo の子から group へ透過が流れ込む");

    song.set_tracks_enabled(&[2], false);
    let mut sched = compile_schedule(&song, &lat, 48_000, 256, RenderScope::Mix).unwrap();
    assert!(!sched.solo.any_solo(&song), "無効な子の solo は数えない");
    sched.solo.resolve(&song);
    assert!(!sched.solo.of(0).0, "無効な子からは透過が流れ込まない");
}
