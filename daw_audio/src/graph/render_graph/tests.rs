use super::*;
use crate::graph::{DeviceLatencies, LoadingDevices, compile_schedule};
use common::model::{
    AudioTap, AuxInputRoute, AuxOutputRoute, FollowerConfig, ModSource, ModSourceKind, NativeDevice, NativeKind,
    PluginInstance, Send, SendMode, TapPoint, TapSource, Track,
};
use common::plugin_format::PluginFormat;
use common::protocol::RenderScope;

fn track(id: u32, f: impl FnOnce(&mut Track)) -> Track {
    let mut t = Track { id, ..Track::default() };
    f(&mut t);
    t
}

fn fx_plugin(id: u64) -> PluginInstance {
    PluginInstance {
        id,
        ..PluginInstance::with_ports(
            format!("test.fx{id}"),
            PluginFormat::Clap,
            common::port_config::PortConfig {
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                has_audio_input: true,
                has_video_input: false,
                has_video_output: false,
            },
        )
    }
}

fn comp_with_sc(id: u64, tap: AudioTap) -> Device {
    Device::Native(NativeDevice { aux_input: Some(AuxInputRoute { tap }), ..NativeDevice::new_added(NativeKind::Comp, id, 1) })
}

/// 旧 pass 2 の op を一通り含む曲:
///
/// - G2 (group) ⊃ { G1 (group) ⊃ { A, B (latency 付き plugin → A に PDC) }, C }
/// - G1 の Comp は C の PostFader をサイドチェインに読む (pass 2 の consumer)
/// - D は Comp で G1 の PostFader を読み (pass 1 の consumer の staging)、plugin で A の PreFx を aux 入力に読む、
///   R1 へ post / pre fader の send を出す
/// - C の plugin の aux 出力が R1 へ流れる (パラアウト)
/// - R1 (return) の出力を envelope follower が聴く
fn busy_song() -> (Song, DeviceLatencies) {
    let mut lat = DeviceLatencies::new();
    lat.insert(21, 128);
    let song = Song {
        tracks: vec![
            track(1, |t| t.parent_group_id = Some(10)),
            track(2, |t| {
                t.parent_group_id = Some(10);
                t.devices = vec![Device::Plugin(fx_plugin(21))];
            }),
            track(3, |t| {
                t.parent_group_id = Some(20);
                t.devices = vec![Device::Plugin(PluginInstance {
                    aux_outputs: vec![None, Some(AuxOutputRoute::to_track(30))],
                    ..fx_plugin(31)
                })];
            }),
            track(4, |t| {
                t.devices = vec![
                    comp_with_sc(40, AudioTap::post_fader(10)),
                    Device::Plugin(PluginInstance {
                        aux_inputs: vec![Some(AuxInputRoute {
                            tap: AudioTap { source: TapSource::Track(1), tap_point: TapPoint::PreFx },
                        })],
                        ..fx_plugin(41)
                    }),
                ];
                t.sends = vec![
                    Send { id: 1, dest_track_id: 30, gain: 0.5, mode: SendMode::PostFader, enabled: true },
                    Send { id: 2, dest_track_id: 30, gain: 0.5, mode: SendMode::PreFader, enabled: true },
                ];
            }),
            track(10, |t| {
                t.parent_group_id = Some(20);
                t.devices = vec![comp_with_sc(100, AudioTap::post_fader(3))];
            }),
            track(20, |_| {}),
            track(30, |t| t.devices = vec![Device::Native(NativeDevice::new_added(NativeKind::Eq, 300, 1))]),
        ],
        mod_sources: vec![ModSource {
            id: 1,
            owner_track_id: 30,
            color: [0.0; 3],
            kind: ModSourceKind::EnvelopeFollower { tap: Some(AudioTap::post_fader(30)), follower: FollowerConfig::default() },
            enabled: true,
        }],
        ..Song::default()
    };
    (song, lat)
}

/// job ごとの到達可能性 (`reach[a][b]` = a から辺を辿って b に着く、a == b を含む)。
fn reachability(g: &RenderGraph) -> Vec<Vec<bool>> {
    let n = g.job_count();
    let mut reach = vec![vec![false; n]; n];
    for (a, row) in reach.iter_mut().enumerate() {
        let mut stack = vec![a as u32];
        while let Some(j) = stack.pop() {
            if !std::mem::replace(&mut row[j as usize], true) {
                stack.extend_from_slice(g.succs_of(j));
            }
        }
    }
    reach
}

/// **正しさの根拠そのもの**: 直列トレース上で同じ資源を読み書きする手の組は、グラフでも必ず T と同じ順に
/// 並ぶ (同じ job の中で前にあるか、前の手の job から辿り着ける)。これが成り立てば、辺を守るどの順で job を
/// 実行しても結果は T の順と bit 一致する。
#[test]
fn 資源が衝突する手の組は直列トレースと同じ順に並ぶ() {
    let (song, lat) = busy_song();
    let sched = compile_schedule(&song, &lat, &LoadingDevices::new(), 48_000, 256, RenderScope::Mix).expect("compile");
    let g = &sched.graph;
    let kinds = |pred: fn(&NodeOp) -> bool| sched.nodes.iter().filter(|op| pred(op)).count();
    assert!(kinds(|op| matches!(op, NodeOp::ApplyDelay { .. })) > 0, "前提: PDC");
    assert!(kinds(|op| matches!(op, NodeOp::SidechainTap { .. })) > 0, "前提: plugin の SC");
    assert!(kinds(|op| matches!(op, NodeOp::NativeSidechainTap { .. })) >= 2, "前提: 内蔵 device の SC");
    assert!(kinds(|op| matches!(op, NodeOp::ParallelOutTap { .. })) > 0, "前提: パラアウト");
    assert!(kinds(|op| matches!(op, NodeOp::MixSend { .. })) >= 2, "前提: send");
    assert!(kinds(|op| matches!(op, NodeOp::EnvelopeFollow { .. })) > 0, "前提: follower");

    // 全手がちょうど 1 回、job の中は T の順。
    let pos: HashMap<Step, usize> = g.trace.iter().enumerate().map(|(i, s)| (*s, i)).collect();
    let mut job_of = vec![u32::MAX; g.trace.len()];
    for j in 0..g.job_count() as u32 {
        let steps = g.steps(j);
        assert!(steps.windows(2).all(|w| pos[&w[0]] < pos[&w[1]]), "job {j} の中が T の順でない");
        for s in steps {
            assert_eq!(std::mem::replace(&mut job_of[pos[s]], j), u32::MAX, "{s:?} が 2 回");
        }
    }
    assert!(job_of.iter().all(|&j| j != u32::MAX), "job に入っていない手がある");

    let res = Resources::new(&song);
    let reach = reachability(g);
    let (mut r1, mut w1, mut r2, mut w2) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut conflicts = 0;
    for (a, &sa) in g.trace.iter().enumerate() {
        res.access(sa, &sched.nodes, &mut r1, &mut w1);
        for (b, &sb) in g.trace.iter().enumerate().skip(a + 1) {
            res.access(sb, &sched.nodes, &mut r2, &mut w2);
            let conflict = w1.iter().any(|x| w2.contains(x) || r2.contains(x)) || r1.iter().any(|x| w2.contains(x));
            if !conflict {
                continue;
            }
            conflicts += 1;
            let (ja, jb) = (job_of[a], job_of[b]);
            assert!(reach[ja as usize][jb as usize], "{sa:?} (job {ja}) → {sb:?} (job {jb}) の順が守られない");
        }
    }
    assert!(conflicts > 20, "衝突の検査が空振りしている: {conflicts}");
}

/// **並列にできるものは並列に並ぶ**: 依存の無い bus (G1 と R1) の chain は、互いに辿り着けない別の job に入る
/// (= 同時に走れる)。重い leaf (B) の処理も、無関係な return (R1) の開始を待たせない。
#[test]
fn 依存の無い_bus_同士は別々の_job_で同時に走れる() {
    let (song, lat) = busy_song();
    let sched = compile_schedule(&song, &lat, &LoadingDevices::new(), 48_000, 256, RenderScope::Mix).expect("compile");
    let g = &sched.graph;
    let job_with = |want: Step| (0..g.job_count() as u32).find(|&j| g.steps(j).contains(&want)).expect("job");
    let group_fx = |track_idx: u32| {
        let k = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: t, .. } if *t == track_idx))
            .expect("ProcessGroupFx");
        job_with(Step::Node(k as u32))
    };
    let reach = reachability(g);
    let (g1, r1, b) = (group_fx(4), group_fx(6), job_with(Step::Process(1)));
    assert!(!reach[g1 as usize][r1 as usize] && !reach[r1 as usize][g1 as usize], "G1 と R1 が直列化されている");
    assert!(!reach[b as usize][r1 as usize], "leaf B が R1 を待たせている");
    assert!(g.roots.len() >= song.tracks.len(), "track 本体は全部最初から走れる: roots {}", g.roots.len());
}

struct NoPark {
    master: bool,
}

impl Park for NoPark {
    fn is_master(&self) -> bool {
        self.master
    }
    fn wake(&self, _: u32) {}
    fn wake_master(&self) {}
    fn finished(&self) {}
    fn park(&self, _: &RenderGraph) -> bool {
        panic!("1 スレッドなら寝る場面は無い");
    }
}

/// master バスへ書く job は callback スレッドだけが取る: worker が取れるものを全部流しても master の合流は残り、
/// callback スレッドがそれを (それだけを) 流して終わる。
#[test]
fn master_バスへの合流は_callback_スレッドだけが流す() {
    let (song, lat) = busy_song();
    let sched = compile_schedule(&song, &lat, &LoadingDevices::new(), 48_000, 256, RenderScope::Mix).expect("compile");
    let (g, nodes) = (&sched.graph, &sched.nodes);
    let writes_master =
        |j: u32| g.steps(j).iter().any(|s| matches!(s, Step::Node(k) if matches!(nodes[*k as usize], NodeOp::Mix { dst: BufRef::Master, .. })));
    g.begin();
    let mut by_worker = Vec::new();
    drain(g, &NoPark { master: false }, &mut |j| by_worker.push(j));
    assert!(!g.is_done() && !by_worker.is_empty());
    assert!(by_worker.iter().all(|&j| !writes_master(j)), "worker が master の合流を取った");
    let mut by_master = Vec::new();
    drain(g, &NoPark { master: true }, &mut |j| by_master.push(j));
    assert!(g.is_done());
    assert!(!by_master.is_empty() && by_master.iter().all(|&j| writes_master(j)), "callback スレッドに残ったのは合流だけ: {by_master:?}");
}

/// 1 スレッドで job を取り合う実行 (`run_graph`) が、全 job を 1 回ずつ、辺の順を守って流す。
#[test]
fn 待ち行列は全_job_を辺の順に_1_回ずつ流す() {
    let (song, lat) = busy_song();
    let sched = compile_schedule(&song, &lat, &LoadingDevices::new(), 48_000, 256, RenderScope::Mix).expect("compile");
    let g = &sched.graph;
    for _ in 0..3 {
        g.begin();
        let mut done = vec![false; g.job_count()];
        let mut order = Vec::new();
        assert!(run_graph(g, &NoPark { master: true }, |j| {
            assert!(!std::mem::replace(&mut done[j as usize], true), "job {j} が 2 回");
            order.push(j);
        }));
        assert!(done.iter().all(|&d| d));
        let at: HashMap<u32, usize> = order.iter().enumerate().map(|(i, j)| (*j, i)).collect();
        for j in 0..g.job_count() as u32 {
            assert!(g.succs_of(j).iter().all(|s| at[&j] < at[s]), "job {j} が後続より後に走った");
        }
    }
}
