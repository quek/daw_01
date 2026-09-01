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

/// RT へ配送する 1 組 (評価計画と、それに合わせて確保済みの RT 状態)。
///
/// `ModRuntime::install` は `Vec::resize` するので **必ず off-thread で**通す。
pub type ModPlanDelivery = (Arc<ModPlan>, ModRuntime);

/// 評価計画を作り、前回と違うときだけ配送物を返す。
#[derive(Debug, Default)]
pub struct ModPlanPublisher {
    last: Option<Arc<ModPlan>>,
    generation: u64,
}

impl ModPlanPublisher {
    /// `song` から評価計画を作る。**世代以外が前回と同じなら `None`** (据え置き)。
    pub fn build(&mut self, song: &Song, sample_rate: u32) -> Option<ModPlanDelivery> {
        let sr = f64::from(sample_rate.max(1));
        // `FromBeat` の anchor を秒へ換算する (テンポマップが要るので呼び側の仕事)。
        #[allow(clippy::cast_precision_loss)]
        let plan = build_plan(song, self.generation + 1, |beat| {
            common::automation::beats_to_samples(song, sample_rate, beat) as f64 / sr
        });
        if let Some(prev) = self.last.as_deref()
            && prev.nodes == plan.nodes
            && prev.slot_ids == plan.slot_ids
            && prev.lane_params == plan.lane_params
        {
            return None;
        }
        self.generation += 1;
        let plan = Arc::new(plan);
        let mut rt = ModRuntime::default();
        rt.install(&plan);
        self.last = Some(Arc::clone(&plan));
        Some((plan, rt))
    }
}

/// 位相表を張る専用スレッドと、その郵便受け。
pub struct ModPhaseTableBuilder {
    request: Arc<Mailbox>,
    done_rx: std::sync::mpsc::Receiver<Arc<ModPhaseTable>>,
}

/// 最新の要求だけ残す郵便受け (古い要求は捨てる — 途中の形は誰も要らない)。
type Mailbox = (std::sync::Mutex<Option<Request>>, std::sync::Condvar);

struct Request {
    plan: Arc<ModPlan>,
    song: Arc<Song>,
    sample_rate: u32,
    length_secs: f64,
}

impl ModPhaseTableBuilder {
    #[must_use]
    pub fn spawn() -> Self {
        let request: Arc<Mailbox> =
            Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
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
    pub fn request(&self, plan: Arc<ModPlan>, song: &Song, sample_rate: u32) {
        if !plan.needs_integration() {
            return;
        }
        let sr = f64::from(sample_rate.max(1));
        #[allow(clippy::cast_precision_loss)]
        let length_secs =
            common::automation::beats_to_samples(song, sample_rate, song.length_beats) as f64 / sr;
        let (lock, cv) = &*self.request;
        if let Ok(mut slot) = lock.lock() {
            *slot = Some(Request {
                plan,
                song: Arc::new(song.clone()),
                sample_rate,
                length_secs,
            });
            cv.notify_one();
        }
    }

    /// 完成した表を受け取る (無ければ `None`)。溜まっていたら最新の 1 枚だけ返す。
    #[must_use]
    pub fn take_finished(&self) -> Option<Arc<ModPhaseTable>> {
        let mut latest = None;
        while let Ok(t) = self.done_rx.try_recv() {
            latest = Some(t);
        }
        latest
    }
}

/// 郵便受けを待って表を張り続ける。`done_tx` が閉じたら (= recv loop が畳まれたら)
/// 終わる。
fn worker_loop(mailbox: &Mailbox, done_tx: &std::sync::mpsc::Sender<Arc<ModPhaseTable>>) {
    while let Some(job) = wait_for_request(mailbox) {
        let table = ModPhaseTable::build(&job.plan, &job.song, job.sample_rate, job.length_secs);
        if done_tx.send(Arc::new(table)).is_err() {
            return;
        }
    }
}

/// 要求が入るまで待つ。lock が毒されたら `None` (= スレッドを畳む)。
fn wait_for_request(mailbox: &Mailbox) -> Option<Request> {
    let (lock, cv) = mailbox;
    let mut slot = lock.lock().ok()?;
    while slot.is_none() {
        slot = cv.wait(slot).ok()?;
    }
    slot.take()
}
