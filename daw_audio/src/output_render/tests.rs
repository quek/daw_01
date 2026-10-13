//! 先読みの検査。描画スレッドは起こさず、「コールバック 1 回 → 描画スレッドが起きて 1 回分の仕事」を同じスレッドで
//! 交互に回す (本番の順序と同じで、決定的)。

use std::collections::HashMap;

use common::device_scope_bridge::{DeviceScopeBridgeHandle, MAX_DEVICE_SCOPES};
use common::model::{Song, Track};
use common::protocol::ProjectKey;
use common::song_index::SongIndex;
use common::timing::SongBounds;

use super::*;
use crate::audio_clip_renderer::AudioClipRenderer;
use crate::engine::{
    EngineCommandSender, EngineShared, PlaybackCommand, ProjectDelivery, ProjectRt, ProjectShared, RecordingLanes,
    RtBundle,
};

const SR: u32 = 48_000;
const FRAMES: usize = 256;
const CH: usize = 2;
const KEY: ProjectKey = ProjectKey(1);

struct Rig {
    stage: OutputStage,
    worker: RenderThread,
    bridge: Arc<AudioBridgeHandle>,
    shared: Arc<SharedState>,
    project: Arc<ProjectShared>,
    buf: Vec<f32>,
    // 生かしておくだけ (落とすと RT 側の受け口が閉じる)。
    _keep: (rtrb::Producer<RtBundle>, rtrb::Producer<ProjectDelivery>),
}

#[allow(clippy::field_reassign_with_default)]
fn rig(tag: &str, ahead: bool) -> Rig {
    let pid = std::process::id();
    let engine = Arc::new(EngineShared::new());
    let (_cmd_tx, cmd_rx) = EngineCommandSender::channel();
    let (mut project_tx, project_rx) = rtrb::RingBuffer::new(8);
    let (project_recycle_tx, project_recycle_rx) = rtrb::RingBuffer::new(8);
    let (device_tx, device_rx) = rtrb::RingBuffer::new(4);
    let (device_recycle_tx, device_recycle_rx) = rtrb::RingBuffer::new(4);
    std::mem::forget((project_recycle_rx, device_tx, device_recycle_rx));
    let local = DeviceRt::new(
        common::process_data::MAX_FRAMES,
        cmd_rx,
        Arc::clone(&engine),
        project_rx,
        project_recycle_tx,
        device_rx,
        device_recycle_tx,
    );
    let bridge = Arc::new(AudioBridgeHandle::create(&format!("daw01_test_ahead_ab_{tag}_{pid}")).unwrap());
    let metrics = Arc::new(MetricsBridgeHandle::create(&format!("daw01_test_ahead_mb_{tag}_{pid}")).unwrap());
    let scope = Arc::new(ScopeBridgeHandle::create(&format!("daw01_test_ahead_sc_{tag}_{pid}")).unwrap());
    let device_scope =
        Arc::new(DeviceScopeBridgeHandle::create(&format!("daw01_test_ahead_ds_{tag}_{pid}")).unwrap());

    // project を 1 つ開く (recv loop の `OpenProject` と同じ手順)。
    let slot = bridge.claim_project_slot(KEY).unwrap();
    let (project, pool_rx, pool_recycle_tx) = ProjectShared::new_with_stretch_rings(KEY, slot);
    let project = Arc::new(project);
    let (mut bundle_tx, bundle_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
    std::mem::forget(recycle_rx);
    let rt = ProjectRt::new(
        common::process_data::MAX_FRAMES,
        Arc::clone(&project),
        bundle_rx,
        recycle_tx,
        pool_rx,
        pool_recycle_tx,
    );
    project_tx.push(ProjectDelivery::Open(Box::new(rt))).ok().unwrap();
    let mut song = Song::default();
    let mut t = Track::default();
    t.id = 1;
    song.tracks.push(t);
    bundle_tx.push(bundle(&Arc::new(song))).ok().unwrap();

    let shared = Arc::new(SharedState::new());
    shared.render_ahead_wanted.store(ahead, Ordering::Release);
    let renderer = OutputRenderer::new(
        local,
        Arc::clone(&shared),
        Arc::clone(&bridge),
        metrics,
        scope,
        device_scope,
        SR,
        CH,
    );
    let (stage, worker) = OutputStage::new(renderer);
    Rig {
        stage,
        worker,
        bridge,
        shared,
        project,
        buf: vec![0.0; FRAMES * CH],
        _keep: (bundle_tx, project_tx),
    }
}

fn bundle(song: &Arc<Song>) -> RtBundle {
    RtBundle {
        song: Some(Arc::clone(song)),
        song_index: Arc::new(SongIndex::build(song)),
        song_bounds: SongBounds::of(Some(song), SR),
        tempo_map: Arc::new(common::tempo_map::TempoMap::from_song(song)),
        schedule: Some(
            crate::graph::compile_schedule(
                song,
                &HashMap::new(),
                &crate::graph::LoadingDevices::new(),
                SR,
                FRAMES as u32,
                common::protocol::RenderScope::Mix,
            )
            .unwrap(),
        ),
        reset_song_scoped_state: false,
        input_delay_replacements: Vec::new(),
        scratch_growth: Some(crate::mixer::ScratchGrowth::new(0, song.tracks.len())),
        launcher_growth: Some(crate::launcher::LauncherGrowth::for_song(song)),
        plane: None,
        follower_maps: None,
        plugin_refs: Arc::new(HashMap::new()),
        preview_sequence: None,
        loop_region: common::model::LoopRegion::default(),
        recording_lanes: Arc::new(RecordingLanes::new()),
        audio_clip_renderer: Arc::new(AudioClipRenderer::empty()),
        device_scope_watch: [0; MAX_DEVICE_SCOPES],
        mod_plan: None,
        mod_phase_table: None,
    }
}

impl Rig {
    /// コールバック 1 回と、それに起こされた描画スレッドの 1 回分。
    fn cycle(&mut self) {
        self.stage.on_callback(&mut self.buf);
        self.worker.step();
    }

    fn renderer_on_callback(&self) -> bool {
        self.stage.inline.is_some()
    }

    fn published_playhead(&self) -> u64 {
        self.bridge.find_project(KEY).unwrap().playhead_samples()
    }

    fn play(&self) {
        self.project.metronome_enabled.store(true, Ordering::Release);
        self.project.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
    }

    fn stop(&self) {
        self.project.playback.store(PlaybackCommand::Stop as u8, Ordering::Release);
    }
}

/// デバイスへ渡した音の中の 2 拍目のクリック (曲の位置 24000) と、そのコールバックの直後に publish された再生位置の
/// ずれ (= 再生位置 − そのコールバックの末尾が曲のどこか)。聞こえている音と再生位置が揃っていれば先読みの有無で同じ。
fn second_click_offset(ahead: bool) -> i64 {
    let mut r = rig(if ahead { "pos_on" } else { "pos_off" }, ahead);
    for _ in 0..4 {
        r.cycle();
    }
    assert_eq!(r.renderer_on_callback(), !ahead, "停止中なので望みどおりの描き手になっている");
    r.play();
    // 渡した音を並べる (L だけ)。クリックの頭 = 1000 サンプル以上の無音の後の最初の非ゼロ。
    let mut silent_run = usize::MAX;
    let mut onsets = Vec::new();
    for k in 0..200 {
        r.cycle();
        let published = r.published_playhead();
        for (i, f) in r.buf.as_chunks::<CH>().0.iter().enumerate() {
            if f[0].abs() > 1e-6 {
                if silent_run >= 1000 {
                    onsets.push((k * FRAMES + i, k, published));
                }
                silent_run = 0;
            } else {
                silent_run = silent_run.saturating_add(1);
            }
        }
    }
    assert!(onsets.len() >= 2, "1 拍目と 2 拍目のクリックが鳴る: {onsets:?}");
    let (o0, _, _) = onsets[0];
    let (o1, k1, published) = onsets[1];
    assert_eq!(o1 - o0, 24_000, "120 BPM の 1 拍 = 24000 サンプル間隔で渡る");
    // コールバック k1 の末尾は曲の位置 24000 + ((k1 + 1) × FRAMES − o1)。
    let end_of_callback = 24_000 + ((k1 + 1) * FRAMES - o1) as i64;
    published as i64 - end_of_callback
}

/// 先読みしても、publish する再生位置 (GUI の表示・映像・MIDI 録音の位置合わせの元) は今デバイスへ渡している音の位置と
/// 一致する (先読みしないときと同じ)。
#[test]
fn 先読みしても再生位置は渡している音と揃う() {
    let off = second_click_offset(false);
    let on = second_click_offset(true);
    assert!((0..=2).contains(&off), "先読みしないとき再生位置はコールバックの末尾 (クリックの立ち上がりの丸め込み): {off}");
    assert_eq!(on, off, "先読みでもずれない (2 周期分を遅らせて publish している)");
}

/// 描き手の切り替えは停止中だけ。再生中に設定を変えても次の停止まで待ち、止まったら替わる。戻すときは描き溜めを出し切る。
#[test]
fn 描き手は停止中にだけ替わる() {
    let mut r = rig("switch", false);
    for _ in 0..4 {
        r.cycle();
    }
    assert!(r.renderer_on_callback());
    r.play();
    for _ in 0..4 {
        r.cycle();
    }
    r.shared.render_ahead_wanted.store(true, Ordering::Release);
    for _ in 0..20 {
        r.cycle();
        assert!(r.renderer_on_callback(), "再生中は替えない");
    }
    r.stop();
    for _ in 0..4 {
        r.cycle();
    }
    assert!(!r.renderer_on_callback(), "止まったら描画スレッドへ渡る");

    r.play();
    for _ in 0..4 {
        r.cycle();
    }
    r.shared.render_ahead_wanted.store(false, Ordering::Release);
    for _ in 0..20 {
        r.cycle();
        assert!(!r.renderer_on_callback(), "再生中は替えない");
    }
    r.stop();
    for _ in 0..(RENDER_AHEAD_PERIODS + 2) {
        r.cycle();
    }
    assert!(r.renderer_on_callback(), "止まったらコールバックへ戻る");
    assert!(r.stage.cons.is_empty(), "描き溜めは出し切ってから自分で描く");
}

/// コールバックと描画スレッドの 1 回分は、先読みの有無・切り替えを含めて確保も解放もしない
/// (`cargo test -p daw_audio --features rt-assert`)。
#[cfg(feature = "rt-assert")]
#[test]
fn コールバックと描画スレッドは確保しない() {
    let mut r = rig("rt", true);
    // 初回の便の取り込みなどの温まりは検査の外。
    for _ in 0..8 {
        r.cycle();
    }
    r.play();
    for _ in 0..8 {
        r.cycle();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..50 {
            r.cycle();
        }
        r.stop();
        r.shared.render_ahead_wanted.store(false, Ordering::Release);
        for _ in 0..8 {
            r.cycle();
        }
        r.play();
        for _ in 0..50 {
            r.cycle();
        }
        r.stop();
        r.shared.render_ahead_wanted.store(true, Ordering::Release);
        for _ in 0..8 {
            r.cycle();
        }
        r.play();
        for _ in 0..50 {
            r.cycle();
        }
    });
    assert!(!r.renderer_on_callback(), "最後は先読みに戻っている");
}
