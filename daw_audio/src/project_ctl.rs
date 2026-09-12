//! recv loop (IPC スレッド) 側の **project (= タブ) ごと**の制御
//! (`docs/plan_project_tabs.md` §3.3)。
//!
//! - [`ProjectCtl`]: 1 project の off-RT 状態 (共有面 / bundle publisher / recycle ring)。
//! - [`open_project`] / [`ProjectCtl::close`]: RT 側の slot ([`ProjectRt`]) を off-thread で
//!   作って / 外して配送する。
//! - [`handle_project_command`]: project 宛 `AudioCommand` の処理。`main.rs::recv_loop` は
//!   宛先の解決とデバイス全体のコマンドだけを持つ (不変条件 9 — `main.rs` の budget)。
//! - `LoadSong` に伴う audio clip renderer の publish と、background decode worker。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::audio_bridge::AudioBridgeHandle;
use common::protocol::{AudioCommand, ProjectKey};

use crate::engine::{
    self, EngineCommand, EngineShared, PluginEntry, ProjectDelivery, ProjectRt, ProjectShared,
    RtBundle,
};
use crate::graph::{DelayLine, Schedule, compile_schedule};
use crate::mod_plan_publish::{ModPhaseTableBuilder, ModPlanPublisher};
use crate::{audio_clip_renderer, launcher, mixer, sampler, song_values, stretch_engine};

/// project ごとの forward ring の深さ (RT が 1 buffer 遅れても人の編集速度では溢れない)。
const BUNDLE_RING_CAP: usize = 8;
/// project ごとの recycle ring の深さ。
const BUNDLE_RECYCLE_CAP: usize = 64;

/// A request for the background decode worker: fully (re)compile the audio
/// schedule for `song`, decoding any source not already cached in the live
/// renderer. `generation` is the schedule version at dispatch — the worker
/// drops its result if a newer `LoadSong` has bumped it, so a slow decode can't
/// clobber a fresher schedule (r.md #7 decode 再設計 B)。
pub struct DecodeJob {
    pub project: Arc<ProjectShared>,
    pub song: Arc<common::model::Song>,
    pub project_dir: Option<std::path::PathBuf>,
    pub generation: u64,
}

/// Background decode worker loop. Owns a dedicated std::thread so large WAV
/// decodes never stall the tokio IPC receive loop. Coalesces queued jobs **per
/// project** to the newest (so a burst of imports doesn't decode intermediate
/// states), reuses already-decoded buffers from the live renderer, decodes only
/// the missing sources, and publishes the full renderer via `ArcSwap` — but only
/// while its generation is still current.
pub fn decode_worker_loop(rx: std::sync::mpsc::Receiver<DecodeJob>, session_sample_rate: u32) {
    while let Ok(first) = rx.recv() {
        // Coalesce to the newest queued song per project so a flurry of
        // imports/edits only decodes the final state, not every intermediate one.
        let mut jobs: Vec<DecodeJob> = vec![first];
        while let Ok(newer) = rx.try_recv() {
            jobs.retain(|j| j.project.key != newer.project.key);
            jobs.push(newer);
        }
        for job in jobs {
            if job.generation != job.project.schedule_generation.load(Ordering::Acquire) {
                continue; // superseded before we started
            }
            let prev = job.project.audio_clip_renderer.load();
            let prev_ref: &audio_clip_renderer::AudioClipRenderer = &prev;
            let full = audio_clip_renderer::compile_audio_schedule(
                &job.song,
                Some(prev_ref),
                job.project_dir.as_deref(),
                session_sample_rate,
                true,
            );
            // Publish only if no newer schedule has landed while we decoded
            // (mutex-guarded so the generation check and the store are atomic).
            publish_audio_clip_schedule(&job.project, job.generation, full, session_sample_rate);
        }
    }
}

/// Publish a freshly compiled renderer for `generation`, but only if no newer
/// schedule has already been published. Serializes the receive loop's reuse-only
/// partial and the decode worker's full renderer through a mutex so a slow
/// decode for generation N can't clobber a newer N+1 that landed during the
/// decode (the bare `schedule_generation` re-check has a TOCTOU window between
/// its load and the `store`). Off the audio thread — the CPAL callback only ever
/// `load()`s the `ArcSwap`, never this mutex (r.md #7 B)。
pub(crate) fn publish_audio_clip_schedule(
    project: &ProjectShared,
    generation: u64,
    renderer: audio_clip_renderer::AudioClipRenderer,
    session_sample_rate: u32,
) {
    let mut last = project
        .last_published_generation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if generation >= *last {
        // r.md #40: schedule を store する **前**に、その schedule が要求する
        // stretch engine を確保して配送する。 RT は buffer の頭で pool を drain
        // してから renderer snapshot を load するので、この順序で「新 schedule の
        // `engine_slot` に対応するエンジンが必ず居る」 が成立する。
        deliver_stretch_engines(project, &renderer, session_sample_rate);
        project.audio_clip_renderer.store(Arc::new(renderer));
        *last = generation;
    }
}

/// 新 schedule が要求する stretch engine のうち **不足分だけ**を確保して RT へ
/// 配送する (`ProjectShared::stretch_pool_tx`)。 pool は grow-only なので、
/// 既に走行中のエンジンには一切触らない (= 無関係な編集で発音中の clip が
/// prime し直しにならない)。 off-thread 専用 (1 個 ~1 MB の確保が走る)。
fn deliver_stretch_engines(
    project: &ProjectShared,
    renderer: &audio_clip_renderer::AudioClipRenderer,
    session_sample_rate: u32,
) {
    // RT が空にして返した配送便をここで捨てる (RT では free しない)。
    if let Ok(mut recycle) = project.stretch_pool_recycle_rx.lock() {
        while recycle.pop().is_ok() {}
    }

    let mut delivered = project
        .delivered_engines_per_track
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Ok(mut tx) = project.stretch_pool_tx.lock() else {
        return;
    };
    // RT 側 (= consumer) が居ないなら作っても捨てるだけ。 1 個 ~1 MB の確保を
    // publish のたびに空振りさせない。
    if tx.is_abandoned() {
        return;
    }
    for (track_idx, &needed) in renderer.engines_per_track.iter().enumerate() {
        // `MAX_TRACKS` を超える track は render されない (`render_master_buffer` が
        // `min(MAX_TRACKS)` で切る) ので、エンジンを作っても無駄。
        if track_idx >= common::audio_bridge::MAX_TRACKS {
            break;
        }
        // `TrackScratch::stretch_engines` の予約容量が上限。 これを超えて配送すると
        // RT 側が取り込めず、「配送済み」 だけが進んで永久に足りない状態になる
        // (`assign_engine_slots` が同じ上限で彩色するので通常は届かない)。
        let needed = needed.min(
            u16::try_from(audio_clip_renderer::MAX_STRETCH_ENGINES_PER_TRACK).unwrap_or(u16::MAX),
        );
        if delivered.len() <= track_idx {
            delivered.resize(track_idx + 1, 0);
        }
        let have = delivered[track_idx];
        if needed <= have {
            continue;
        }
        let mut engines = Vec::with_capacity(usize::from(needed - have));
        for _ in have..needed {
            let Some(engine) = stretch_engine::StretchEngine::new(session_sample_rate) else {
                tracing::error!(track_idx, "stretch engine の確保に失敗 (OOM?)");
                break;
            };
            engines.push(engine);
        }
        if engines.is_empty() {
            continue;
        }
        let added = u16::try_from(engines.len()).unwrap_or(u16::MAX);
        if tx
            .push(engine::StretchPoolDelivery { track_idx, engines })
            .is_err()
        {
            // ring 満杯 = RT が drain していない (停止中 / 起動前)。 次の publish で
            // 再送されるよう配送済みカウントは進めない。
            tracing::warn!(track_idx, "stretch engine pool ring full; 次の publish で再送");
            continue;
        }
        delivered[track_idx] = have.saturating_add(added);
    }
}

/// forward ring への bundle 送出 (drop-oldest 化)。 rtrb の producer は
/// consumer 側を追い出せないので、 ring full 時は新しい bundle を `parked` に
/// 退避し (旧 parked = superseded は **ここ (off-thread)** で drop)、 次の
/// 送出 / recv イテレーションで再 push する。 RT が正常に drain していれば
/// full は起きない — これは「RT が遅くても最新編集が最終的に必ず届く」保険
/// (plan §4: drop-newest でなく drop-oldest)。
pub struct BundlePublisher {
    tx: rtrb::Producer<RtBundle>,
    /// これまでに配送した per-track scratch の本数 (`RtBundle::scratch_growth`)。
    /// 増える方向にだけ動かす — 減らしても走行状態の移送が要るだけで得が無い。
    delivered_scratch: usize,
    pub(crate) parked: Option<RtBundle>,
    /// r.md #89: クロス変調の評価計画の publish 状態 (内容が変わったときだけ載せる)。
    mod_plans: ModPlanPublisher,
    /// 直近 topology compile に使った `buffer_frames` (leaf 宛 sidechain tap
    /// の 1-buffer 補償量)。 実測値との drift を検知して再 compile する。
    last_compiled_frames: Option<u32>,
}

impl BundlePublisher {
    pub fn new(tx: rtrb::Producer<RtBundle>) -> Self {
        Self {
            tx,
            delivered_scratch: 0,
            parked: None,
            mod_plans: ModPlanPublisher::default(),
            last_compiled_frames: None,
        }
    }

    /// parked bundle があれば ring へ再 push を試みる。
    pub fn flush(&mut self) {
        if let Some(bundle) = self.parked.take()
            && let Err(rtrb::PushError::Full(back)) = self.tx.push(bundle)
        {
            self.parked = Some(back);
        }
    }

    pub fn send(&mut self, bundle: RtBundle) {
        self.flush();
        if let Err(rtrb::PushError::Full(mut newest)) = self.tx.push(bundle) {
            // ring full。 旧 parked は superseded だが、 `schedule` は snapshot
            // ではなく delta なので、 捨てる前に `supersede` で newest へ
            // 畳み込む (RT 側 `refresh_bundle` の coalescing と同じ規約 —
            // 畳み込まないと topology 更新がここで失われる)。 畳み込み後の
            // 残骸だけを drop する — off-thread。
            if let Some(older) = self.parked.take() {
                drop(newest.supersede(older));
            }
            self.parked = Some(newest);
        }
    }
}

/// schedule compile に使う buffer frames (= leaf 宛 sidechain tap の 1-buffer
/// staging 補償量)。 CPAL callback が実測値を `last_buffer_frames` に publish
/// する。 未計測 (stream 稼働前の初回 publish のみ) は WASAPI 共有モード既定の
/// 10ms 周期を仮定し、 最初の callback 後に recv loop の drift check が実測値で
/// 再 compile する。
pub fn resolve_buffer_frames(engine_shared: &EngineShared, sample_rate: u32) -> u32 {
    let max = common::process_data::MAX_FRAMES as u32;
    match engine_shared.last_buffer_frames.load(Ordering::Acquire) {
        0 => (sample_rate / 100).clamp(64, max),
        measured => measured.min(max),
    }
}

/// `input_delay_per_track` が `TrackScratch` の prealloc (1s) を超える病的
/// ケース用の置換 DelayLine を off-thread で確保する (install 時に RT が
/// swap するだけで済むように)。 全 track が prealloc 内なら空 Vec。
fn build_input_delay_replacements(schedule: &Schedule) -> Vec<Option<DelayLine>> {
    let mut any = false;
    let repl: Vec<Option<DelayLine>> = schedule
        .input_delay_per_track
        .iter()
        .map(|&d| {
            let need = d as usize + 1;
            if d > 0 && need > mixer::INPUT_DELAY_PREALLOC_SAMPLES {
                any = true;
                Some(DelayLine::with_capacity(need))
            } else {
                None
            }
        })
        .collect();
    if any { repl } else { Vec::new() }
}

/// `publish_bundle` の topology 引数。 呼び出し側が意図を名前で述べる。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// 値のみの更新 (SetTrackVolume 等)。 schedule は載せない = RT は現行
    /// schedule (走行状態込み) を据え置く (§5 D: 値更新で `compile_schedule`
    /// を走らせない)。
    Unchanged,
    /// topology 変更 (LoadSong / buffer_frames drift)。 schedule を off-thread
    /// で再 compile して載せる。
    ///
    /// `reset_song_scoped_state = true` は「このタブに別のファイルが読み込まれた」
    /// (`Song::project_id` が変わった) の意で、RT に走行状態 (PDC ring /
    /// follower envelope / per-track input delay line) を **引き継がず捨てさせる**。
    /// これらの移送キーは Song スコープの id なので project を跨ぐと別物同士が
    /// 一致してしまう。
    Recompile { reset_song_scoped_state: bool },
}

/// 1 project の off-RT 制御状態 (`docs/plan_project_tabs.md` §3.3)。
pub struct ProjectCtl {
    pub shared: Arc<ProjectShared>,
    pub publisher: BundlePublisher,
    /// RT が superseded にした bundle の戻り口 (`Drop` は housekeeping で off-thread)。
    pub bundle_recycle_rx: rtrb::Consumer<RtBundle>,
    /// MIDI Capture の試聴シーケンスの差し替え世代 (`sampler::PreviewSequence`)。
    pub preview_seq_generation: u64,
}

impl ProjectCtl {
    pub fn key(&self) -> ProjectKey {
        self.shared.key
    }

    /// 現在の mirrors (plugin_refs / preview_sequence) + `song` で `RtBundle` を組んで
    /// RT へ配送する。 `shared.song` mirror もここで更新する (off-thread 読者用)。
    pub fn publish_bundle(
        &mut self,
        engine_shared: &EngineShared,
        song: Option<Arc<common::model::Song>>,
        sample_rate: u32,
        topology: Topology,
        phase_tables: &ModPhaseTableBuilder,
    ) {
        self.shared.song.store(song.clone());
        let tempo_map = match song.as_deref() {
            Some(s) => common::tempo_map::TempoMap::from_song(s),
            None => common::tempo_map::TempoMap::from_song(&common::model::Song::default()),
        };
        let (schedule, input_delay_replacements) = if topology != Topology::Unchanged {
            let buffer_frames = resolve_buffer_frames(engine_shared, sample_rate);
            self.publisher.last_compiled_frames = Some(buffer_frames);
            let sched = match song.as_deref() {
                // compile 失敗は empty schedule (silent master) に fallback —
                // 壊れた graph は謎の音ではなく無音として聴こえる方が診断しやすい。
                Some(s) => match compile_schedule(
                    s,
                    &self.shared.device_latencies.load(),
                    sample_rate,
                    buffer_frames,
                ) {
                    Ok(sc) => sc,
                    Err(e) => {
                        tracing::warn!(?e, project = self.shared.key.0, "graph compile failed; master goes silent");
                        Schedule::empty()
                    }
                },
                None => Schedule::empty(),
            };
            let repl = build_input_delay_replacements(&sched);
            (Some(sched), repl)
        } else {
            (None, Vec::new())
        };
        // r.md #89: クロス変調の評価計画。`Song::mod_sources` / `mod_routings` /
        // automation lane から決まるので **値のみ更新でも変わりうる** (schedule と
        // 違って topology 限定ではない)。作るのは安いが、内容が変わっていないのに
        // 載せると RT が毎 buffer 位相を捨てて張り直すので、前回と同じなら載せない。
        let mod_plan = song
            .as_deref()
            .and_then(|sg| self.publisher.mod_plans.build(sg, sample_rate));
        // 位相表は曲長ぶんの刻みループなので **必ず off-thread**。構築中は旧表 +
        // 閉形式シードで凌ぎ、完成したら housekeeping が次の便で載せる。
        if let (Some((plan, _)), Some(sg)) = (mod_plan.as_ref(), song.as_ref()) {
            phase_tables.request(self.shared.key, Arc::clone(plan), sg, sample_rate);
        }
        // per-track scratch は **曲が要る本数だけ** off-thread で確保して、song と
        // 同じ便で届ける (`RtBundle::scratch_growth` の doc — 無条件 `MAX_TRACKS` は
        // タブ 1 枚あたり ~14 MB になる)。
        let needed = song
            .as_deref()
            .map_or(0, |s| s.tracks.len().min(common::audio_bridge::MAX_TRACKS));
        let scratch_growth = if needed > self.publisher.delivered_scratch {
            self.publisher.delivered_scratch = needed;
            Some((0..needed).map(|_| mixer::TrackScratch::new()).collect())
        } else {
            None
        };
        self.publisher.send(RtBundle {
            song,
            tempo_map,
            scratch_growth,
            schedule,
            mod_plan,
            mod_phase_table: None,
            reset_song_scoped_state: matches!(
                topology,
                Topology::Recompile {
                    reset_song_scoped_state: true
                }
            ),
            input_delay_replacements,
            plugin_refs: self.shared.plugin_refs.load_full(),
            preview_sequence: self.shared.preview_sequence.load_full(),
        });
    }

    /// 現在の song をそのまま値のみ bundle で送り直す (mirror が変わったとき)。
    pub fn republish(
        &mut self,
        engine_shared: &EngineShared,
        sample_rate: u32,
        phase_tables: &ModPhaseTableBuilder,
    ) {
        let song = self.shared.song.load_full();
        self.publish_bundle(engine_shared, song, sample_rate, Topology::Unchanged, phase_tables);
    }

    /// Apply `f` to a clone of the current song and publish the result as a
    /// **値のみ** bundle (schedule 再 compile なし — §5 D)。 mixer-strip 変更は
    /// user-driven (slider drag rate) なので clone は IPC スレッドで許容。
    fn update_song_values<F>(
        &mut self,
        engine_shared: &EngineShared,
        sample_rate: u32,
        phase_tables: &ModPhaseTableBuilder,
        f: F,
    ) where
        F: FnOnce(&mut common::model::Song),
    {
        let snapshot = self.shared.song.load();
        let Some(song) = snapshot.as_deref() else {
            return;
        };
        let mut next = song.clone();
        f(&mut next);
        drop(snapshot);
        self.publish_bundle(
            engine_shared,
            Some(Arc::new(next)),
            sample_rate,
            Topology::Unchanged,
            phase_tables,
        );
    }

    /// 周期処理: RT が superseded にした bundle の drop、parked の再送、完成した
    /// 位相表の配送、buffer_frames drift の再 compile。
    pub fn housekeeping(
        &mut self,
        engine_shared: &EngineShared,
        sample_rate: u32,
        phase_tables: &ModPhaseTableBuilder,
        finished_table: Option<Arc<common::mod_graph::ModPhaseTable>>,
    ) {
        while let Ok(old) = self.bundle_recycle_rx.pop() {
            drop(old);
        }
        self.publisher.flush();
        // r.md #89: off-thread で張り終えた位相表を RT へ載せる (構築中は旧表 +
        // 閉形式シードで凌いでいる)。plan と別便なのは、表の構築が曲長ぶんの
        // 刻みループで、plan の配送を待たせたくないから (設計正本 §2.4)。
        if let Some(table) = finished_table {
            let song = self.shared.song.load_full();
            self.publisher.send(RtBundle {
                tempo_map: match song.as_deref() {
                    Some(s) => common::tempo_map::TempoMap::from_song(s),
                    None => common::tempo_map::TempoMap::from_song(&common::model::Song::default()),
                },
                song,
                schedule: None,
                reset_song_scoped_state: false,
                input_delay_replacements: Vec::new(),
                // 位相表だけの便。scratch は `publish_bundle` が song と同じ便で運ぶ。
                scratch_growth: None,
                plugin_refs: self.shared.plugin_refs.load_full(),
                preview_sequence: self.shared.preview_sequence.load_full(),
                mod_plan: None,
                mod_phase_table: Some(table),
            });
        }
        // leaf 宛 sidechain tap の 1-buffer 補償量 (= 実測 buffer frames) が
        // compile 時の仮定から変わっていたら topology を再 publish する
        // (初回 publish が stream 実測前に走った場合の是正)。
        if let Some(compiled) = self.publisher.last_compiled_frames
            && resolve_buffer_frames(engine_shared, sample_rate) != compiled
        {
            let song = self.shared.song.load_full();
            if song.is_some() {
                self.publish_bundle(
                    engine_shared,
                    song,
                    sample_rate,
                    // 同 project の再 compile なので走行状態は引き継ぐ。
                    Topology::Recompile {
                        reset_song_scoped_state: false,
                    },
                    phase_tables,
                );
            }
        }
    }
}

/// `OpenProject`: 共有面 + RT slot を off-thread で作り、telemetry slot を claim して
/// RT へ配送する。`None` = slot 満杯 (`MAX_PROJECTS`)。既に開いている key は冪等。
pub fn open_project(
    key: ProjectKey,
    projects: &mut HashMap<ProjectKey, ProjectCtl>,
    engine_shared: &EngineShared,
    bridge: &AudioBridgeHandle,
    project_tx: &mut rtrb::Producer<ProjectDelivery>,
) -> bool {
    if projects.contains_key(&key) {
        tracing::info!(project = key.0, "OpenProject: already open (idempotent)");
        return true;
    }
    let Some(slot) = bridge.claim_project_slot(key) else {
        tracing::warn!(project = key.0, "OpenProject: no free telemetry slot (MAX_PROJECTS)");
        return false;
    };
    let (shared, pool_rx, pool_recycle_tx) = ProjectShared::new_with_stretch_rings(key, slot);
    let shared = Arc::new(shared);
    let (bundle_tx, bundle_rx) = rtrb::RingBuffer::<RtBundle>::new(BUNDLE_RING_CAP);
    let (bundle_recycle_tx, bundle_recycle_rx) =
        rtrb::RingBuffer::<RtBundle>::new(BUNDLE_RECYCLE_CAP);
    let rt = ProjectRt::new(
        common::process_data::MAX_FRAMES,
        Arc::clone(&shared),
        bundle_rx,
        bundle_recycle_tx,
        pool_rx,
        pool_recycle_tx,
    );
    if project_tx.push(ProjectDelivery::Open(Box::new(rt))).is_err() {
        // ring 満杯 = RT が居ない / 詰まっている。slot を返して失敗にする。
        bridge.release_project_slot(slot);
        tracing::error!(project = key.0, "OpenProject: delivery ring full");
        return false;
    }
    let mut map = (**engine_shared.projects.load()).clone();
    map.insert(key, Arc::clone(&shared));
    engine_shared.projects.store(Arc::new(map));
    projects.insert(
        key,
        ProjectCtl {
            shared,
            publisher: BundlePublisher::new(bundle_tx),
            bundle_recycle_rx,
            preview_seq_generation: 0,
        },
    );
    tracing::info!(project = key.0, slot, "project slot opened");
    true
}

/// `CloseProject`: RT から外す便を送り、off-RT ミラーから消す。RT が返した
/// `ProjectRt` は housekeeping ([`reap_closed_projects`]) が drop して telemetry slot を
/// 解放する (RT が書き終える前に slot を空けない)。
pub fn close_project(
    key: ProjectKey,
    projects: &mut HashMap<ProjectKey, ProjectCtl>,
    engine_shared: &EngineShared,
    project_tx: &mut rtrb::Producer<ProjectDelivery>,
) {
    let Some(ctl) = projects.remove(&key) else {
        tracing::debug!(project = key.0, "CloseProject: not open");
        return;
    };
    let mut map = (**engine_shared.projects.load()).clone();
    map.remove(&key);
    engine_shared.projects.store(Arc::new(map));
    if project_tx.push(ProjectDelivery::Close(key)).is_err() {
        tracing::error!(project = key.0, "CloseProject: delivery ring full");
    }
    // ctl (publisher / recycle ring) はここで drop。RT が持つ `ProjectRt` 側の
    // ring 片割れは abandoned になるだけで panic しない。
    drop(ctl);
    tracing::info!(project = key.0, "project slot closed");
}

/// RT が撤去した `ProjectRt` を off-thread で drop し、telemetry slot を解放する。
pub fn reap_closed_projects(
    project_recycle_rx: &mut rtrb::Consumer<Box<ProjectRt>>,
    bridge: &AudioBridgeHandle,
) {
    while let Ok(rt) = project_recycle_rx.pop() {
        let slot = rt.telemetry_slot;
        drop(rt);
        bridge.release_project_slot(slot);
    }
}

/// 鍵盤プレビューの note-on/off を送る対象 track の Vec index を、 その project の
/// 現 song snapshot から track id で引く。 song 未ロード / id 不在 / `MAX_TRACKS`
/// 超過は `None` (= プレビュー drop)。 id ベースなので GUI 側の track 並べ替えと
/// race しない (= `SetTrackVolume` 等と同じ方針)。
pub(crate) fn preview_track_index(project: &ProjectShared, track_id: u32) -> Option<usize> {
    let snapshot = project.song.load();
    let song = snapshot.as_deref()?;
    song.tracks
        .iter()
        .position(|t| t.id == track_id)
        .filter(|&i| i < engine::MAX_TRACKS)
}

/// project 宛 `AudioCommand` の処理 (`main.rs::recv_loop` が宛先を解いてから呼ぶ)。
/// オフライン描画 (Export / Loudness / Bounce) は `offline_jobs` が別に受ける。
#[allow(clippy::too_many_arguments)]
pub fn handle_project_command(
    ctl: &mut ProjectCtl,
    cmd: AudioCommand,
    engine_shared: &EngineShared,
    session_sample_rate: u32,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    decode_tx: &std::sync::mpsc::Sender<DecodeJob>,
    phase_tables: &ModPhaseTableBuilder,
) {
    let shared = Arc::clone(&ctl.shared);
    let key = shared.key;
    match cmd {
        // r.md #118: `PlayContinue` は launcher を再シードしない (engine 側で分岐)。
        AudioCommand::Play { .. } => {
            tracing::info!(project = key.0, "received Play");
            shared.playback.store(engine::PlaybackCommand::Play as u8, Ordering::Release);
        }
        AudioCommand::PlayContinue { .. } => {
            tracing::info!(project = key.0, "received PlayContinue");
            shared
                .playback
                .store(engine::PlaybackCommand::PlayContinue as u8, Ordering::Release);
        }
        AudioCommand::Stop { .. } => {
            tracing::info!(project = key.0, "received Stop");
            shared.playback.store(engine::PlaybackCommand::Stop as u8, Ordering::Release);
        }
        AudioCommand::SetLoop { mut region, .. } => {
            // IPC は信頼境界。 NaN / 負値の拍位置は samples_per_beat 換算を
            // 壊すので store 前に正規化する (LoadSong の sanitize_ranges と同旨)。
            region.sanitize();
            shared.loop_region.store(Arc::new(region));
        }
        AudioCommand::SeekTo { samples, .. } => {
            // playhead を IPC 受信スレッドから直接書かない。
            // audio thread も buffer 末で playhead を store するため、両者が
            // 同一 atomic を別スレッドから書く race になり、Stop 直後の開始
            // 位置への巻き戻しが in-flight buffer の advance に上書きされて
            // 停止位置から再生されるバグを生む。seek 要求は
            // pending_seek に積み、audio thread が process_buffer 冒頭で
            // swap 消費して playhead に反映する (playhead の writer を audio
            // thread 単独に保つ)。ruler click / Stop 復帰の双方ともこの経路。
            shared.pending_seek.store(samples, Ordering::Release);
            tracing::info!(project = key.0, samples, "received SeekTo");
        }
        AudioCommand::LoadSong { mut song, .. } => {
            load_song(ctl, &mut song, engine_shared, session_sample_rate, decode_tx, phase_tables);
        }
        AudioCommand::SetMasterGain { gain, .. } => {
            // render (`render_master_buffer`) が読む — live / export 共通。
            // +6 dB (amp 2.0) まで許可 (r.md #11、 GUI clamp と同 SSoT)。
            let clamped = gain.clamp(0.0, common::model::MAX_TRACK_GAIN);
            shared.master_gain.store(clamped.to_bits(), Ordering::Relaxed);
        }
        AudioCommand::SetDeviceLatency { device_id, samples, .. } => {
            set_device_latency(ctl, device_id, samples, engine_shared, session_sample_rate, phase_tables);
        }
        AudioCommand::OpenPluginShmem { device_id, shmem_id, token, .. } => {
            // v29: 配置 (どの track のどの位置か) は Song 側の
            // `PluginInstance::id` が SSoT なので、 ここでは device_id →
            // shmem の対応を登録するだけ。 map rebuild は off-thread
            // (snapshot-copy-mutate-publish)、 handle は entry が持つ
            // (旧 Box::leak の解消)。
            match common::process_data::ProcessDataHandle::open(&shmem_id) {
                Ok(handle) => {
                    let entry = Arc::new(PluginEntry::new(device_id, token, handle));
                    let mut map: engine::PluginRefs = (**shared.plugin_refs.load()).clone();
                    map.insert(device_id, entry);
                    shared.plugin_refs.store(Arc::new(map));
                    ctl.republish(engine_shared, session_sample_rate, phase_tables);
                    tracing::info!(project = key.0, device_id, ?token, "plugin shmem registered");
                }
                Err(e) => {
                    tracing::error!(error = ?e, project = key.0, device_id, "failed to open plugin shmem");
                }
            }
        }
        AudioCommand::ClosePluginShmem { device_id, .. } => {
            let mut map: engine::PluginRefs = (**shared.plugin_refs.load()).clone();
            let removed = map.remove(&device_id);
            shared.plugin_refs.store(Arc::new(map));
            ctl.republish(engine_shared, session_sample_rate, phase_tables);
            // 旧 entry (shmem mapping) は RT が新 bundle を install して
            // recycle が drain された時点で off-thread unmap される
            // (drain の上限は housekeeping のタイマが保証)。
            crate::hold_released_entry_for_test(removed);
            tracing::info!(project = key.0, device_id, "plugin shmem dropped");
        }
        // 値のみの Song 更新 (mixer strip / send / arm / bpm / 拍子)。
        // 宛先は安定 id、クランプと適用は `song_values::apply` が SSoT。
        // schedule は再 compile しない (§5 D) — RT は snapshot を live-read する。
        cmd @ (AudioCommand::SetTrackVolume { .. }
        | AudioCommand::SetTrackPan { .. }
        | AudioCommand::SetTrackMuted { .. }
        | AudioCommand::SetTrackSolo { .. }
        | AudioCommand::SetTrackStrip { .. }
        | AudioCommand::SetMasterStrip { .. }
        | AudioCommand::SetTrackArmed { .. }
        | AudioCommand::SetSendGain { .. }
        | AudioCommand::SetSendEnabled { .. }
        | AudioCommand::SetChainGain { .. }
        | AudioCommand::SetChainPan { .. }
        | AudioCommand::SetChainMuted { .. }
        | AudioCommand::SetChainSolo { .. }
        | AudioCommand::SetParallelOutGain { .. }
        | AudioCommand::SetParallelGainMatch { .. }
        | AudioCommand::SetParallelSplitFreq { .. }
        | AudioCommand::SetParallelActiveChain { .. }
        | AudioCommand::SetParallelSelectorFade { .. }
        | AudioCommand::SetSongBpm { .. }
        | AudioCommand::SetSongTimeSigNumerator { .. }) => {
            ctl.update_song_values(engine_shared, session_sample_rate, phase_tables, |s| {
                song_values::apply(&cmd, s);
            });
        }
        // r.md #87: クリップランチャーの操作。発火の判断には Song が要るので
        // ここでは audio thread へ積むだけ (`launcher::ipc` が唯一の口)。
        cmd @ (AudioCommand::LaunchCell { .. }
        | AudioCommand::LaunchCellFrom { .. }
        | AudioCommand::LaunchScene { .. }
        | AudioCommand::RephaseLauncherRows { .. }
        | AudioCommand::StopRow { .. }
        | AudioCommand::StopAllRows { .. }
        | AudioCommand::SwitchRowToArranger { .. }
        | AudioCommand::SwitchAllToArranger { .. }) => {
            launcher::ipc::dispatch(cmd, cmd_tx);
        }
        AudioCommand::StartRecording { preroll_samples, .. } => {
            // r.md #51: 録音セッションの開始。 `recording_requested` は
            // 曲末 auto-stop の抑止と `recording_live` の publish に使う。
            // preroll > 0 なら process_buffer が「dispatch / clip render skip +
            // metronome のみ render」 の count-in ループに入り、 0 到達で
            // 通常再生に復帰する (= その瞬間に recording_live が立つ)。
            shared.preroll_total_samples.store(preroll_samples, Ordering::Release);
            shared.preroll_remaining_samples.store(preroll_samples, Ordering::Release);
            shared.recording_requested.store(true, Ordering::Release);
            tracing::info!(project = key.0, preroll_samples, "received StartRecording");
        }
        AudioCommand::StopRecording { .. } => {
            // r.md #51: 録音セッションの終了 (パンチアウト / 停止 / count-in
            // 取り消し)。 transport はここでは止めない — パンチアウトは
            // 再生を続けるのが参照 DAW 共通の挙動で、停止は `Stop` の仕事。
            shared.recording_requested.store(false, Ordering::Release);
            shared.preroll_remaining_samples.store(0, Ordering::Release);
            shared.preroll_total_samples.store(0, Ordering::Release);
            tracing::info!(project = key.0, "received StopRecording");
        }
        AudioCommand::SetMetronomeEnabled { enabled, .. } => {
            // GUI が transport bar の metronome toggle を切り替え。 audio thread は
            // 次 buffer から `render_metronome` の有効無効を切り替える。
            shared.metronome_enabled.store(enabled, Ordering::Release);
        }
        AudioCommand::PreviewNoteOn { track_id, pitch, velocity, .. } => {
            // 鍵盤レーン click のプレビュー (gui_01 #055)。 GUI は track id を
            // 送る。 ここで engine の現 song snapshot から Vec index を
            // 引いて EngineCommand に載せ替える。 解決は IPC スレッド上 =
            // RT 外。 song 未ロード / id 不在なら drop (= 無音)。
            if let Some(track) = preview_track_index(&shared, track_id) {
                let _ = cmd_tx.send(EngineCommand::PreviewNoteOn {
                    project: key,
                    track,
                    pitch,
                    velocity: f64::from(velocity) / 127.0,
                });
            }
        }
        AudioCommand::PreviewNoteOff { track_id, pitch, .. } => {
            if let Some(track) = preview_track_index(&shared, track_id) {
                let _ = cmd_tx.send(EngineCommand::PreviewNoteOff { project: key, track, pitch });
            }
        }
        AudioCommand::SetRecordingLanes { lanes, .. } => {
            // Phase 4 Step C-2: GUI が「現在 recording 中の lane」 セットを
            // 送ってきた。 ArcSwap で snapshot を replace し、 audio thread
            // は次 buffer から `fill_track_param_ramps` で該当 lane の
            // curve eval を skip する。 lock-free / 0 allocation on audio thread。
            let set: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
                lanes.into_iter().collect();
            shared.recording_lanes.store(Arc::new(set));
        }
        AudioCommand::SetProjectDir { dir, .. } => {
            // `compile_audio_schedule` が `AudioSourcePath::ProjectRelative`
            // を `<project_dir>/samples/<...>` に解決するのに使う。
            // `None` for unsaved projects.
            shared.project_dir.store(dir.as_ref().map(|p| Arc::new(p.clone())));
            tracing::info!(project = key.0, ?dir, "project_dir updated");
        }
        // MIDI Capture の試聴 (`docs/plan_global_sampler.md`)。
        cmd @ (AudioCommand::PreviewSequence { .. } | AudioCommand::PreviewSequenceStop { .. }) => {
            if sampler::handle_project_command(cmd, &shared, &mut ctl.preview_seq_generation) {
                ctl.republish(engine_shared, session_sample_rate, phase_tables);
            }
        }
        other => {
            // OpenProject / CloseProject / SetScopeProject / offline jobs / デバイス全体の
            // command はここへ来ない (`recv_loop` が先に振り分ける)。
            tracing::warn!(?other, project = key.0, "unexpected command in project handler");
        }
    }
}

/// `LoadSong`: 値域を正規化し、同じ slot で別ファイルが開かれたかを判定して、
/// audio clip schedule (reuse-only 部分) + topology bundle を publish する。
fn load_song(
    ctl: &mut ProjectCtl,
    song: &mut common::model::Song,
    engine_shared: &EngineShared,
    session_sample_rate: u32,
    decode_tx: &std::sync::mpsc::Sender<DecodeJob>,
    phase_tables: &ModPhaseTableBuilder,
) {
    let shared = Arc::clone(&ctl.shared);
    // IPC は信頼境界なので、 受信した song の値域を store 前に
    // 正規化 (bpm/time_sig/length/loop/framerate を有限・正に)。
    // これで下流の divisor (samples_per_beat 等) が NaN / 0 /
    // 負値で壊れない。 idempotent。
    song.sanitize_ranges();
    // このタブに別のファイルが読み込まれたか (`Song::project_id` は v24 で
    // 導入されたプロジェクト同一性の SSoT。New で採番し save/load で
    // 保持される)。Song 内の id (track / device / audio_source /
    // ModSource …) はどれも project ごとに 1 から再採番されるので、
    // 変わった瞬間に **それらを key にした状態は全部無効** になる。
    let project_switched = shared
        .loaded_project_id
        .swap(song.project_id, Ordering::AcqRel)
        != song.project_id;
    if project_switched {
        tracing::info!(
            project = shared.key.0,
            project_id = song.project_id,
            "song identity switched in slot; dropping song-scoped engine state"
        );
        // 旧 song の device の shmem 参照。 破棄は daw_gui の
        // ClosePluginShmem に依存していたが、 その列挙元 (帳簿) に
        // 取りこぼしがあると前 song の instance を掴んだまま
        // 音を出してしまう。 device_id も song ごとに 1 から
        // 再採番されるので、 ここで一括して捨てる (新 song の
        // 分は SetSlotPlugin → SlotPluginLoaded → OpenPluginShmem
        // で必ず後から届く)。
        shared.plugin_refs.store(Arc::new(HashMap::new()));
        // track_id keyed。 非空のまま持ち越すと新 song の同番号
        // track の automation が bypass されたままになる。
        shared.recording_lanes.store(Arc::new(std::collections::HashSet::new()));
        // device_id keyed の PDC 入力も同じく song スコープ。
        // 持ち越すと新 song の同番号 device が、 まだ何も報告して
        // いないのに前 song の latency で補償される。
        shared
            .device_latencies
            .store(Arc::new(crate::graph::DeviceLatencies::new()));
    }
    let project_dir_g = shared.project_dir.load();
    let project_dir: Option<std::path::PathBuf> =
        project_dir_g.as_ref().map(|arc| (**arc).clone());
    // Bump the schedule version so any in-flight decode for an older
    // song is discarded when it tries to publish (r.md #7 B)。
    let generation = shared.schedule_generation.fetch_add(1, Ordering::AcqRel) + 1;
    let song = Arc::new(std::mem::take(song));
    // Phase 1: publish a reuse-only schedule synchronously. Sources
    // already decoded in the live renderer are Arc-cloned (no
    // decode), so BPM change / edit / scrub re-compile with zero
    // decode and never block the receive loop. Sources not yet
    // decoded are left out — their events stay silent until the
    // worker fills them in.
    let prev = shared.audio_clip_renderer.load();
    let prev_ref: &audio_clip_renderer::AudioClipRenderer = &prev;
    let partial = audio_clip_renderer::compile_audio_schedule(
        &song,
        Some(prev_ref),
        project_dir.as_deref(),
        session_sample_rate,
        false,
    );
    // main 側: 未 decode 判定は id 一致でなく origin (解決済み絶対
    // パス) 一致で行う。 r.md #40 側: publish が stretch engine pool の
    // 配送も担うので session SR が要る。
    let needs_decode =
        audio_clip_renderer::has_undecoded_sources(&song, &partial, project_dir.as_deref());
    drop(prev);
    publish_audio_clip_schedule(&shared, generation, partial, session_sample_rate);
    // topology publish: routing schedule + tempo map を off-thread
    // で compile して RT へ wait-free 配送 (shared.song もここで更新)。
    ctl.publish_bundle(
        engine_shared,
        Some(Arc::clone(&song)),
        session_sample_rate,
        Topology::Recompile {
            reset_song_scoped_state: project_switched,
        },
        phase_tables,
    );
    // Phase 2: hand off to the background worker for full decode of
    // any missing source. Skipped when everything was reusable
    // (= BPM change / edit / scrub → decode ゼロ で即完結)。
    if needs_decode {
        let _ = decode_tx.send(DecodeJob {
            project: shared,
            song,
            project_dir,
            generation,
        });
    }
}

/// `SetDeviceLatency`: PDC の入力更新 → 値が変わったときだけ再 compile。
fn set_device_latency(
    ctl: &mut ProjectCtl,
    device_id: u64,
    samples: u32,
    engine_shared: &EngineShared,
    session_sample_rate: u32,
    phase_tables: &ModPhaseTableBuilder,
) {
    let shared = Arc::clone(&ctl.shared);
    // 報告値はプラグイン (= 信頼できない外部コード) が返した u32 で、
    // そのまま DelayLine の容量になる。 実在するプラグインの latency は
    // 高々数百 ms なので、 10 秒相当で頭打ちにして異常値で確保を
    // 暴走させない (FFI / IPC 境界の値域検証)。
    let max_samples = session_sample_rate.saturating_mul(10);
    let samples = if samples > max_samples {
        tracing::warn!(
            device_id,
            samples,
            max_samples,
            "plugin reported an implausible latency; clamping"
        );
        max_samples
    } else {
        samples
    };
    // PDC の入力更新。 表は off-RT でしか読まれない (compile 時のみ)
    // ので、 copy-on-write で差し替えて schedule を組み直す。
    // 値が変わらないなら再 compile しない (plugin host は load ごとに
    // 0 でも必ず報告してくるので、 無条件 recompile は起動時に
    // device 数ぶんの無駄な再 compile を生む)。
    let current = shared.device_latencies.load();
    let unchanged = match (current.get(&device_id), samples) {
        (None, 0) => true,
        (Some(&prev), s) => prev == s,
        _ => false,
    };
    if unchanged {
        return;
    }
    let mut next = (**current).clone();
    if samples == 0 {
        next.remove(&device_id);
    } else {
        next.insert(device_id, samples);
    }
    drop(current);
    tracing::info!(project = shared.key.0, device_id, samples, "device latency updated (PDC 再 compile)");
    shared.device_latencies.store(Arc::new(next));
    // song が届く前の報告もあり得る (plugin load の方が速い) —
    // その場合は表だけ更新し、 次の LoadSong の compile が拾う。
    let song = shared.song.load_full();
    if song.is_some() {
        ctl.publish_bundle(
            engine_shared,
            song,
            session_sample_rate,
            // 同 project の再 compile。 DelayLine / FollowerSlot の
            // 走行状態は引き継ぐ (曲は変わっていない)。
            Topology::Recompile {
                reset_song_scoped_state: false,
            },
            phase_tables,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_bundle() -> RtBundle {
        RtBundle {
            song: None,
            tempo_map: common::tempo_map::TempoMap::from_song(&common::model::Song::default()),
            schedule: None,
            reset_song_scoped_state: false,
            input_delay_replacements: Vec::new(),
            scratch_growth: None,
            plugin_refs: Arc::new(std::collections::HashMap::new()),
            preview_sequence: None,
            mod_plan: None,
            mod_phase_table: None,
        }
    }

    /// BundlePublisher の drop-oldest: ring が full のとき新しい bundle が
    /// park され (最新優先、 superseded parked は off-thread drop)、 space が
    /// できたら flush で届く。
    #[test]
    fn bundle_publisher_parks_newest_on_full_ring() {
        let (tx, mut rx) = rtrb::RingBuffer::<RtBundle>::new(1);
        let mut publisher = BundlePublisher::new(tx);
        publisher.send(empty_bundle()); // fills the 1-slot ring
        publisher.send(empty_bundle()); // full → parked
        publisher.send(empty_bundle()); // full → parked (previous parked dropped here)
        assert!(publisher.parked.is_some());
        // consumer drains one slot → flush delivers the parked newest.
        assert!(rx.pop().is_ok());
        publisher.flush();
        assert!(publisher.parked.is_none());
        assert!(rx.pop().is_ok());
    }

    /// `docs/plan_project_tabs.md` §3.3: open / close は telemetry slot と off-RT
    /// ミラーを対で更新し、RT へ Open / Close 便を送る。満杯なら open は失敗する。
    #[test]
    fn open_close_project_は_slot_とミラーを対で更新する() {
        let engine = EngineShared::new();
        let bridge = AudioBridgeHandle::create(&format!(
            "daw01_test_ctl_{}",
            std::process::id()
        ))
        .unwrap();
        let (mut project_tx, mut project_rx) = rtrb::RingBuffer::new(4);
        let mut projects = HashMap::new();
        assert!(open_project(ProjectKey(1), &mut projects, &engine, &bridge, &mut project_tx));
        assert!(open_project(ProjectKey(1), &mut projects, &engine, &bridge, &mut project_tx), "冪等");
        assert_eq!(projects.len(), 1);
        assert!(engine.project(ProjectKey(1)).is_some());
        assert!(bridge.find_project(ProjectKey(1)).is_some());
        assert!(matches!(project_rx.pop(), Ok(ProjectDelivery::Open(_))));

        close_project(ProjectKey(1), &mut projects, &engine, &mut project_tx);
        assert!(projects.is_empty());
        assert!(engine.project(ProjectKey(1)).is_none());
        assert!(matches!(project_rx.pop(), Ok(ProjectDelivery::Close(ProjectKey(1)))));
        // slot は RT が返すまで生きている (reap で解放)。
        assert!(bridge.find_project(ProjectKey(1)).is_some());
    }

    /// `docs/plan_project_tabs.md` §3.3: telemetry slot は **RT が `ProjectRt` を
    /// 返してから**解放する。close の時点で空けると、RT がまだ書いている面を次の
    /// タブが claim して、閉じたタブの playhead / メーターが新しいタブに出る。
    #[test]
    fn reap_は_rt_が返してから_telemetry_slot_を解放する() {
        let bridge = AudioBridgeHandle::create(&format!(
            "daw01_test_reap_{}",
            std::process::id()
        ))
        .unwrap();
        let slot = bridge.claim_project_slot(ProjectKey(1)).unwrap();
        let (shared, pool_rx, pool_recycle_tx) =
            ProjectShared::new_with_stretch_rings(ProjectKey(1), slot);
        let (_bundle_tx, bundle_rx) = rtrb::RingBuffer::new(1);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(1);
        let rt = ProjectRt::new(
            common::process_data::MAX_FRAMES,
            Arc::new(shared),
            bundle_rx,
            recycle_tx,
            pool_rx,
            pool_recycle_tx,
        );
        let (mut returned_tx, mut returned_rx) = rtrb::RingBuffer::<Box<ProjectRt>>::new(2);

        // RT がまだ返していない間は解放しない。
        reap_closed_projects(&mut returned_rx, &bridge);
        assert!(bridge.find_project(ProjectKey(1)).is_some(), "返る前は slot が生きている");

        // 返ってきたら解放し、同じ slot を次のタブが使える。
        returned_tx.push(Box::new(rt)).ok().unwrap();
        reap_closed_projects(&mut returned_rx, &bridge);
        assert!(bridge.find_project(ProjectKey(1)).is_none(), "返ったら解放される");
        assert_eq!(bridge.claim_project_slot(ProjectKey(2)), Some(slot), "空いた slot を再利用");
    }
}
