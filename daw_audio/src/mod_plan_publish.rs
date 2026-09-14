//! r.md #89: クロス変調の評価計画 (`ModPlan`) と位相表 (`ModPhaseTable`) を
//! **off-thread で作って RT へ配送する**側。
//!
//! 設計正本 `docs/plan_rmd_88_89_cross_modulation.md` §2.4 / §4-6。
//!
//! 2 つを分けてあるのは構築コストが 3 桁違うから:
//!
//! - [`ModPlanPublisher`] — plan は `Song::mod_sources` の DFS なので μs オーダー。
//!   IPC スレッドで同期に作ってよい。ただし **内容が変わったときだけ**配送する
//!   (毎便載せると RT が毎 buffer 位相を捨てて張り直すことになる)。
//! - [`ModPhaseTableBuilder`] — 表は曲頭から曲末まで 64 サンプル刻みでループを
//!   回す (5 分の曲で 20 万刻み)。IPC スレッドで同期に張るとツマミを動かすたびに
//!   パイプが詰まるので、**最新の要求だけ残す郵便受け + 専用スレッド**にする。
//!   構築中は旧表 + 閉形式シードで凌ぐ。

use std::sync::Arc;

use common::mod_graph::{ModPhaseTable, ModPlan, ModRuntime, build_plan};
use common::model::Song;
use common::protocol::ProjectKey;

use crate::mod_tick::ModTickBuffers;

/// RT へ配送する 1 組 (評価計画と、それに合わせて確保済みの RT 状態と器)。
///
/// `ModRuntime::install` は `Vec::resize` し、器 ([`ModTickBuffers`]) は plan の大きさで確保するので
/// **必ず off-thread で**作る ([`Self::new`])。RT の差し替え ([`crate::mod_tick::ModTickRunner::install`])
/// は旧い 1 組を同じ型で返す (recycle で off-thread に落とす)。
#[derive(Debug)]
pub struct ModPlanDelivery {
    pub plan: Arc<ModPlan>,
    pub rt: ModRuntime,
    pub bufs: ModTickBuffers,
}

impl ModPlanDelivery {
    /// `plan` を走らせる RT 状態と器を確保する (off-thread)。
    #[must_use]
    pub fn new(plan: Arc<ModPlan>) -> Self {
        let mut rt = ModRuntime::default();
        rt.install(&plan);
        let bufs = ModTickBuffers::for_plan(&plan);
        Self { plan, rt, bufs }
    }
}

/// 評価計画を作り、前回と違うときだけ配送物を返す。
#[derive(Debug, Default)]
pub struct ModPlanPublisher {
    last: Option<Arc<ModPlan>>,
    generation: u64,
    /// 直近に計画を作った song と sample rate。同じ song (同じ `Arc`) の再配送は作るまでもなく同じ計画なので作らない
    /// (`Weak` なので中身は延命しないが、確保は残るのでアドレスが別の song に再利用されることもない)。
    built_from: Option<(std::sync::Weak<Song>, u32)>,
}

impl ModPlanPublisher {
    /// `song` から評価計画を作る。**世代以外が前回と同じなら `None`** (据え置き)。
    pub fn build(&mut self, song: &Arc<Song>, sample_rate: u32) -> Option<ModPlanDelivery> {
        if self.built_from.as_ref().is_some_and(|(s, sr)| std::ptr::eq(s.as_ptr(), Arc::as_ptr(song)) && *sr == sample_rate) {
            return None;
        }
        self.built_from = Some((Arc::downgrade(song), sample_rate));
        let sr = f64::from(sample_rate.max(1));
        // `FromBeat` の anchor を秒へ換算する (テンポマップが要るので呼び側の仕事)。
        #[allow(clippy::cast_precision_loss)]
        let plan = build_plan(song, self.generation + 1, |beat| {
            common::automation::beats_to_samples(song, sample_rate, beat) as f64 / sr
        });
        // 世代以外の全部を比べる (深さの群が変わっただけの編集 — 深さを動かす変調やレーンの追加 — も載せる。
        // lane の置き場は位置で持つので、track の並べ替えで位置が変わった plan も載せ直す)。
        if let Some(prev) = self.last.as_deref()
            && prev.nodes == plan.nodes
            && prev.slot_ids == plan.slot_ids
            && prev.lane_params == plan.lane_params
            && prev.depth_groups == plan.depth_groups
        {
            return None;
        }
        self.generation += 1;
        let plan = Arc::new(plan);
        self.last = Some(Arc::clone(&plan));
        Some(ModPlanDelivery::new(plan))
    }

    /// 直近に配送した評価計画 (まだ無ければ `None`)。[`crate::mod_tick::FollowerMaps`] を
    /// schedule の差し替えに合わせて作り直すときに使う。
    #[must_use]
    pub fn latest(&self) -> Option<&Arc<ModPlan>> {
        self.last.as_ref()
    }
}

/// 位相表を張る専用スレッドと、その郵便受け。
pub struct ModPhaseTableBuilder {
    request: Arc<Mailbox>,
    done_rx: std::sync::mpsc::Receiver<(ProjectKey, Arc<ModPhaseTable>)>,
}

/// project ごとに最新の要求だけ残す郵便受け (古い要求は捨てる — 途中の形は誰も要らない)。
/// 複数 project (`docs/plan_project_tabs.md`) でも 1 スレッドで順に張る。
type Mailbox = (
    std::sync::Mutex<std::collections::VecDeque<Request>>,
    std::sync::Condvar,
);

struct Request {
    project: ProjectKey,
    plan: Arc<ModPlan>,
    song: Arc<Song>,
    sample_rate: u32,
    length_secs: f64,
}

impl ModPhaseTableBuilder {
    #[must_use]
    pub fn spawn() -> Self {
        let request: Arc<Mailbox> = Arc::new((
            std::sync::Mutex::new(std::collections::VecDeque::new()),
            std::sync::Condvar::new(),
        ));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mailbox = Arc::clone(&request);
        // spawn 失敗 (スレッド枯渇) は「表が張られない」= 閉形式シードに倒れる
        // だけで、音は出続ける。起動を止める理由にはしない。
        if std::thread::Builder::new()
            .name("mod-phase-table".into())
            .spawn(move || worker_loop(&mailbox, &done_tx))
            .is_err()
        {
            tracing::warn!("位相表スレッドを起こせませんでした (閉形式シードに倒れます)");
        }
        Self { request, done_rx }
    }

    /// 表の張り直しを要求する。
    ///
    /// 積分が要らない plan (= rate を変調していない曲) では何もしない —
    /// **既存曲のコストはゼロ**。
    pub fn request(&self, project: ProjectKey, plan: Arc<ModPlan>, song: &Arc<Song>, sample_rate: u32) {
        if !plan.needs_integration() {
            return;
        }
        let sr = f64::from(sample_rate.max(1));
        #[allow(clippy::cast_precision_loss)]
        let length_secs =
            common::automation::beats_to_samples(song, sample_rate, song.length_beats) as f64 / sr;
        let (lock, cv) = &*self.request;
        if let Ok(mut queue) = lock.lock() {
            // 同じ project の古い要求は捨てる (最新だけ残す)。
            queue.retain(|r| r.project != project);
            queue.push_back(Request {
                project,
                plan,
                // `publish_bundle` が持っている `Arc<Song>` をそのまま共有する。
                // deep clone すると、ツマミを動かすたびに曲まるごとの複製が
                // IPC スレッドで走る。
                song: Arc::clone(song),
                sample_rate,
                length_secs,
            });
            cv.notify_one();
        }
    }

    /// 完成した表を受け取る (無ければ空)。同じ project で溜まっていたら最新の 1 枚だけ。
    #[must_use]
    pub fn take_finished(&self) -> Vec<(ProjectKey, Arc<ModPhaseTable>)> {
        let mut latest: Vec<(ProjectKey, Arc<ModPhaseTable>)> = Vec::new();
        while let Ok((key, t)) = self.done_rx.try_recv() {
            latest.retain(|(k, _)| *k != key);
            latest.push((key, t));
        }
        latest
    }
}

/// 郵便受けを待って表を張り続ける。`done_tx` が閉じたら (= recv loop が畳まれたら)
/// 終わる。
fn worker_loop(
    mailbox: &Mailbox,
    done_tx: &std::sync::mpsc::Sender<(ProjectKey, Arc<ModPhaseTable>)>,
) {
    while let Some(job) = wait_for_request(mailbox) {
        let table = ModPhaseTable::build(&job.plan, &job.song, job.sample_rate, job.length_secs);
        if done_tx.send((job.project, Arc::new(table))).is_err() {
            return;
        }
    }
}

/// 要求が入るまで待つ。lock が毒されたら `None` (= スレッドを畳む)。
fn wait_for_request(mailbox: &Mailbox) -> Option<Request> {
    let (lock, cv) = mailbox;
    let mut queue = lock.lock().ok()?;
    while queue.is_empty() {
        queue = cv.wait(queue).ok()?;
    }
    queue.pop_front()
}
