//! 1 buffer の処理の依存グラフと、その RT 実行の待ち行列 (`docs/plan_parallel_graph.md`)。
//!
//! 基準の順序は **直列トレース** `T = [Process(0..n)] ++ [nodes (ProcessTrack 以外)]` (= 旧 pass 1 → pass 2)。
//! T の上で同じ資源を読み書きする手の相対順だけを辺にするので、辺を守る任意の順で実行した結果は T の順と
//! bit 一致する。compile 時 (off-thread) に [`RenderGraph::build`] で組み、RT は [`RenderGraph::begin`] /
//! [`run_graph`] で流す。参照実装: Ardour `libs/ardour/graph.cc` (`Graph::trigger` / `Graph::run_one`)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use common::model::{Device, Song, plugins};

use crate::graph::schedule::{BufRef, MASTER_OWNER, NodeOp};

/// 直列トレースの 1 手。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step {
    /// track `i` の `process_track_owned` (旧 pass 1)。
    Process(u32),
    /// `Schedule::nodes[k]` (旧 pass 2)。
    Node(u32),
}

/// job の中身と後続 (`RenderGraph::steps` / `succs` の範囲)。
#[derive(Debug, Clone, Copy)]
struct Job {
    steps: (u32, u32),
    succs: (u32, u32),
    preds: u32,
}

/// ready queue の未書き込みの席。
const EMPTY: u32 = u32::MAX;

/// 走れるようになった job の待ち行列 (容量 = 積まれうる job の数の配列 + `tail` 予約 + `head` CAS)。1 buffer で各 job は
/// 高々 1 回しか積まれないので、席は予約してから書く。
#[derive(Debug, Default)]
struct ReadyQueue {
    slots: Vec<AtomicU32>,
    head: AtomicU32,
    tail: AtomicU32,
}

impl ReadyQueue {
    fn with_capacity(n: usize) -> Self {
        Self { slots: (0..n).map(|_| AtomicU32::new(EMPTY)).collect(), ..Self::default() }
    }

    /// 1 つのスレッドだけが呼ぶ (buffer の頭)。
    fn reset(&self) {
        let used = self.tail.load(Ordering::Relaxed) as usize;
        for s in &self.slots[..used.min(self.slots.len())] {
            s.store(EMPTY, Ordering::Relaxed);
        }
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
    }

    fn push(&self, job: u32) {
        let at = self.tail.fetch_add(1, Ordering::SeqCst);
        if let Some(slot) = self.slots.get(at as usize) {
            slot.store(job, Ordering::SeqCst);
        }
    }

    /// 予約済みで未書き込みの席に当たったら `None` (書き手がすぐ書く — [`Self::has_work`] は真)。
    fn pop(&self) -> Option<u32> {
        loop {
            let h = self.head.load(Ordering::SeqCst);
            if h >= self.tail.load(Ordering::SeqCst) {
                return None;
            }
            let job = self.slots.get(h as usize)?.load(Ordering::SeqCst);
            if job == EMPTY {
                return None;
            }
            if self.head.compare_exchange(h, h + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return Some(job);
            }
        }
    }

    fn has_work(&self) -> bool {
        self.head.load(Ordering::SeqCst) < self.tail.load(Ordering::SeqCst)
    }
}

/// [`RenderGraph::finish`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// 待ちが無くなった job のうち、呼び側がそのまま続けて実行する 1 つ。
    pub next: Option<u32>,
    /// 共有の待ち行列に積んだ数 (起こす runner の数)。
    pub pushed: u32,
    /// callback スレッド専用の待ち行列に積んだ ([`RenderGraph`] の `on_master`)。
    pub for_master: bool,
}

/// 1 buffer の処理の依存グラフ。構造は compile 時に固まり、RT は作業領域 (atomic) だけを書く。
#[derive(Debug, Default)]
pub struct RenderGraph {
    /// 直列トレース T (pool が無いときはこの順に回す)。
    pub trace: Vec<Step>,
    /// job ごとに連続した手 (job の中は T の順)。
    steps: Vec<Step>,
    jobs: Vec<Job>,
    succs: Vec<u32>,
    /// 最初から走れる job のうち、どの runner が取ってもよいもの。
    roots: Vec<u32>,
    /// **master バスへ書く job** は callback スレッドだけが実行する。master バスはグラフの後で callback スレッドが
    /// master の段 (fx chain / 音量 / Limiter / 出力への書き出し) で読むので、合流もそこで行えば書いた値が冷えない。
    /// 最後の track を終えた worker に続けて合流させると、合流 (曲の出力全部を読む重い直列の手) が起きたばかりで
    /// 冷えた core に回り、手そのものが遅くなる (leaf 64 本 / 1024 frame で 48 µs → 67〜80 µs 実測)。
    on_master: Vec<bool>,
    /// 最初から走れる `on_master` の job。
    master_roots: Vec<u32>,
    // ---- RT の作業領域 (1 buffer ごとに [`Self::begin`] で戻す) ----
    pending: Vec<AtomicU32>,
    ready: ReadyQueue,
    master_ready: ReadyQueue,
    /// 最初から走れる job (`roots`) の次に取る位置。roots は数も並びも compile 時に決まっているので、queue に
    /// 積まずに `fetch_add` で取らせる (失敗しない = 取り合いで再試行しない。track 本体の大半がこれ)。
    next_root: AtomicU32,
    /// まだ終わっていない **後続の無い** job の数。全 job は後続の無い job の祖先なので、これが 0 = 全 job が終わった。
    /// job ごとに全 runner が書く数を作らない (書くのは後続の無い job を終えたときだけ)。
    sinks_left: AtomicU32,
    sink_count: u32,
}

/// 資源の番号: `Scratch(i) = i`、`Program(i) = n + i`、master の program = `2n`、master バス = `2n + 1`、
/// 持ち主の分からない device = `2n + 2` (直列化するだけの受け皿)。
struct Resources {
    n: usize,
    /// device id → 持ち主の program の資源番号。
    device_owner: HashMap<u64, usize>,
}

impl Resources {
    fn new(song: &Song) -> Self {
        let n = song.tracks.len();
        let mut device_owner = HashMap::new();
        let mut add = |devices: &[Device], res: usize| {
            for p in plugins(devices) {
                device_owner.insert(p.id, res);
            }
        };
        for (i, t) in song.tracks.iter().enumerate() {
            add(&t.devices, n + i);
        }
        add(&song.master_fx_chain, 2 * n);
        Self { n, device_owner }
    }

    fn count(&self) -> usize {
        2 * self.n + 3
    }

    fn scratch(&self, i: u32) -> Option<usize> {
        ((i as usize) < self.n).then_some(i as usize)
    }

    fn program(&self, owner: u32) -> Option<usize> {
        if owner == MASTER_OWNER {
            return Some(2 * self.n);
        }
        ((owner as usize) < self.n).then_some(self.n + owner as usize)
    }

    fn master_bus(&self) -> usize {
        2 * self.n + 1
    }

    fn device(&self, id: u64) -> usize {
        self.device_owner.get(&id).copied().unwrap_or(2 * self.n + 2)
    }

    /// tap / 合流の読み元の資源。
    fn buf(&self, b: BufRef) -> Option<usize> {
        match b {
            BufRef::TrackScratch(i) | BufRef::PreFaderScratch(i) | BufRef::PreFxScratch(i) => self.scratch(i),
            BufRef::ChainPostFx { owner, .. }
            | BufRef::ChainPostFader { owner, .. }
            | BufRef::ParallelInput { owner, .. }
            | BufRef::ParallelOutput { owner, .. } => self.program(owner),
            BufRef::Master => Some(self.master_bus()),
            BufRef::Pooled(_) => None,
        }
    }

    /// 手 `step` が読む資源と書く資源 (`docs/plan_parallel_graph.md` §2.1 の表)。
    fn access(&self, step: Step, nodes: &[NodeOp], reads: &mut Vec<usize>, writes: &mut Vec<usize>) {
        reads.clear();
        writes.clear();
        let node = match step {
            Step::Process(i) => {
                writes.extend(self.scratch(i).into_iter().chain(self.program(i)));
                return;
            }
            Step::Node(k) => &nodes[k as usize],
        };
        match node {
            NodeOp::ProcessTrack { .. } => {}
            NodeOp::Mix { srcs, dst } | NodeOp::MixAdditive { srcs, dst } => {
                let dst = match *dst {
                    BufRef::TrackScratch(t) => self.scratch(t),
                    BufRef::Master => Some(self.master_bus()),
                    _ => None,
                };
                if dst.is_some() {
                    reads.extend(srcs.iter().filter_map(|&(b, _)| match b {
                        BufRef::TrackScratch(s) => self.scratch(s),
                        _ => None,
                    }));
                    writes.extend(dst);
                }
            }
            NodeOp::ProcessGroupFx { track_idx, .. } => {
                writes.extend(self.scratch(*track_idx).into_iter().chain(self.program(*track_idx)));
            }
            NodeOp::ApplyDelay { buf, .. } => {
                if let BufRef::TrackScratch(i) = *buf {
                    writes.extend(self.scratch(i));
                }
            }
            NodeOp::SidechainTap { src, device_id, .. } => {
                reads.extend(self.buf(*src));
                writes.push(self.device(*device_id));
            }
            NodeOp::NativeSidechainTap { src, owner, .. } => {
                reads.extend(self.buf(*src));
                writes.extend(self.program(*owner));
            }
            NodeOp::ParallelOutTap { device_id, dst_track, .. } => {
                reads.push(self.device(*device_id));
                writes.extend(self.scratch(*dst_track));
            }
            // solo の表 (`SoloTables`) は program の外にある読むだけの表なので資源にしない。
            NodeOp::MixSend { src, dst, .. } => {
                reads.extend(self.buf(*src));
                if let BufRef::TrackScratch(d) = *dst {
                    writes.extend(self.scratch(d));
                }
            }
            NodeOp::EnvelopeFollow { src, .. } => reads.extend(self.buf(*src)),
        }
        // 読んで書く資源は「書く」として数える (自分自身への辺を張らない)。
        reads.retain(|r| !writes.contains(r));
    }
}

/// T の手ごとの前駆 (T 上の手の番号、昇順・重複なし)。
fn step_preds(trace: &[Step], nodes: &[NodeOp], res: &Resources) -> Vec<Vec<u32>> {
    const NONE: u32 = u32::MAX;
    let mut last_writer = vec![NONE; res.count()];
    let mut readers: Vec<Vec<u32>> = vec![Vec::new(); res.count()];
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    let mut preds = Vec::with_capacity(trace.len());
    for (s, &step) in trace.iter().enumerate() {
        let s = s as u32;
        res.access(step, nodes, &mut reads, &mut writes);
        let mut p: Vec<u32> = Vec::new();
        for &r in &reads {
            if last_writer[r] != NONE {
                p.push(last_writer[r]);
            }
            readers[r].push(s);
        }
        for &w in &writes {
            if last_writer[w] != NONE {
                p.push(last_writer[w]);
            }
            p.extend(readers[w].drain(..).filter(|&r| r != s));
            last_writer[w] = s;
        }
        p.sort_unstable();
        p.dedup();
        preds.push(p);
    }
    preds
}

impl RenderGraph {
    /// `nodes` (compile 済みの op 列) と `song` から組む (off-thread)。`enabled[i]` = track `i` が実効的に有効か —
    /// r.md #131: 無効トラックの `Process` は直列トレースに載せない (job にならない = worker を起こさない)。
    #[must_use]
    pub fn build(song: &Song, nodes: &[NodeOp], enabled: &[bool]) -> Self {
        let res = Resources::new(song);
        let trace: Vec<Step> = (0..song.tracks.len() as u32)
            .filter(|&i| enabled.get(i as usize).copied().unwrap_or(true))
            .map(Step::Process)
            .chain(
                nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, op)| !matches!(op, NodeOp::ProcessTrack { .. }))
                    .map(|(k, _)| Step::Node(k as u32)),
            )
            .collect();
        let preds = step_preds(&trace, nodes, &res);
        let mut succ_count = vec![0u32; trace.len()];
        for p in preds.iter().flatten() {
            succ_count[*p as usize] += 1;
        }
        // 鎖の縮約: 前駆がちょうど 1 つで、その前駆の後続もちょうど 1 つなら前駆の job に畳む
        // (前駆はその job の末尾に居る — 前駆の後続は自分だけなので)。
        let mut job_of = vec![0u32; trace.len()];
        let mut job_steps: Vec<Vec<Step>> = Vec::new();
        for (s, p) in preds.iter().enumerate() {
            if let [only] = p[..]
                && succ_count[only as usize] == 1
            {
                job_of[s] = job_of[only as usize];
                job_steps[job_of[s] as usize].push(trace[s]);
            } else {
                job_of[s] = job_steps.len() as u32;
                job_steps.push(vec![trace[s]]);
            }
        }
        let mut edges: Vec<(u32, u32)> = preds
            .iter()
            .enumerate()
            .flat_map(|(s, p)| p.iter().map(move |&q| (q, s)))
            .map(|(q, s)| (job_of[q as usize], job_of[s]))
            .filter(|(a, b)| a != b)
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        let on_master = job_steps
            .iter()
            .map(|js| {
                js.iter().any(|&s| {
                    res.access(s, nodes, &mut reads, &mut writes);
                    writes.contains(&res.master_bus())
                })
            })
            .collect();
        Self::from_jobs(trace, job_steps, &edges, on_master)
    }

    fn from_jobs(trace: Vec<Step>, job_steps: Vec<Vec<Step>>, edges: &[(u32, u32)], on_master: Vec<bool>) -> Self {
        let n_jobs = job_steps.len();
        let mut preds = vec![0u32; n_jobs];
        for &(_, b) in edges {
            preds[b as usize] += 1;
        }
        let mut steps = Vec::with_capacity(trace.len());
        let mut jobs = Vec::with_capacity(n_jobs);
        let mut succs = Vec::with_capacity(edges.len());
        let mut e = 0;
        for (j, js) in job_steps.into_iter().enumerate() {
            let s0 = steps.len() as u32;
            steps.extend(js);
            let e0 = succs.len() as u32;
            while e < edges.len() && edges[e].0 as usize == j {
                succs.push(edges[e].1);
                e += 1;
            }
            jobs.push(Job { steps: (s0, steps.len() as u32), succs: (e0, succs.len() as u32), preds: preds[j] });
        }
        let (master_roots, roots): (Vec<u32>, Vec<u32>) =
            (0..n_jobs as u32).filter(|&j| preds[j as usize] == 0).partition(|&j| on_master[j as usize]);
        let sink_count = jobs.iter().filter(|job: &&Job| job.succs.0 == job.succs.1).count() as u32;
        let master_jobs = on_master.iter().filter(|&&m| m).count();
        Self {
            trace,
            steps,
            succs,
            pending: (0..n_jobs).map(|_| AtomicU32::new(0)).collect(),
            // 積まれるのは前駆のある job だけ。
            ready: ReadyQueue::with_capacity(n_jobs - roots.len() - master_roots.len()),
            master_ready: ReadyQueue::with_capacity(master_jobs),
            jobs,
            roots,
            on_master,
            master_roots,
            next_root: AtomicU32::new(0),
            sinks_left: AtomicU32::new(0),
            sink_count,
        }
    }

    #[must_use]
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// どの runner が取ってもよい、最初から走れる job の数 (buffer の頭で起こす worker の数の上限)。
    #[must_use]
    pub fn root_count(&self) -> u32 {
        self.roots.len() as u32
    }

    /// job `j` の手 (T の順)。
    #[must_use]
    pub fn steps(&self, j: u32) -> &[Step] {
        let (a, b) = self.jobs[j as usize].steps;
        &self.steps[a as usize..b as usize]
    }

    #[cfg(test)]
    fn succs_of(&self, j: u32) -> &[u32] {
        let (a, b) = self.jobs[j as usize].succs;
        &self.succs[a as usize..b as usize]
    }

    // ---- RT ----

    /// buffer の頭で待ちを戻す (前駆の無い job は `roots` から直接取らせる)。**runner を起こす前に** 1 つの
    /// スレッドだけが呼ぶ。RT 安全: 確保なし (atomic の store だけ)。
    pub fn begin(&self) {
        for (p, job) in self.pending.iter().zip(&self.jobs) {
            p.store(job.preds, Ordering::Relaxed);
        }
        self.ready.reset();
        self.master_ready.reset();
        for &j in &self.master_roots {
            self.master_ready.push(j);
        }
        self.next_root.store(0, Ordering::Relaxed);
        self.sinks_left.store(self.sink_count, Ordering::SeqCst);
    }

    /// 走れる job を 1 つ取る: (callback スレッドなら専用の待ち行列、) まだ取られていない root、積まれている job の
    /// 順。予約済みで未書き込みの席に当たったら `None` (書き手がすぐ書く — [`Self::has_work`] は真なので呼び側は
    /// 寝ずに見直す)。
    pub fn pop(&self, master: bool) -> Option<u32> {
        if master && let Some(job) = self.master_ready.pop() {
            return Some(job);
        }
        // root は数が決まっているので fetch_add で取る (取り過ぎた分は範囲外なので捨てるだけ)。
        if self.next_root.load(Ordering::SeqCst) < self.roots.len() as u32 {
            let r = self.next_root.fetch_add(1, Ordering::SeqCst);
            if let Some(&job) = self.roots.get(r as usize) {
                return Some(job);
            }
        }
        self.ready.pop()
    }

    /// 未取得の job がある (root の残り / 積まれた job、未書き込みの席を含む)。`master` = callback スレッド専用の
    /// 待ち行列も数える。
    #[must_use]
    pub fn has_work(&self, master: bool) -> bool {
        (master && self.master_ready.has_work())
            || self.next_root.load(Ordering::SeqCst) < self.roots.len() as u32
            || self.ready.has_work()
    }

    /// 全 job が終わった。
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.sinks_left.load(Ordering::SeqCst) == 0
    }

    /// job `j` を終えた (`master` = 呼び側が callback スレッド): 後続の待ちを減らし、待ちが無くなった job のうち
    /// 呼び側が実行してよい 1 つは積まずに返し (そのまま続けて実行する)、残りを積む。
    pub fn finish(&self, j: u32, master: bool) -> Finished {
        let mut out = Finished { next: None, pushed: 0, for_master: false };
        let (a, b) = self.jobs[j as usize].succs;
        if a == b {
            self.sinks_left.fetch_sub(1, Ordering::SeqCst);
            return out;
        }
        for &s in &self.succs[a as usize..b as usize] {
            if self.pending[s as usize].fetch_sub(1, Ordering::SeqCst) != 1 {
                continue;
            }
            let on_master = self.on_master[s as usize];
            if out.next.is_none() && (master || !on_master) {
                out.next = Some(s);
            } else if on_master {
                self.master_ready.push(s);
                out.for_master = true;
            } else {
                self.ready.push(s);
                out.pushed += 1;
            }
        }
        out
    }
}

/// runner (pool の worker / callback スレッド) の寝起き。
pub trait Park {
    /// この runner が callback スレッド (`on_master` の job を実行する側) か。
    fn is_master(&self) -> bool;
    /// 寝ている runner を最大 `n` 人起こす (job を `n` 個積んだ)。
    fn wake(&self, n: u32);
    /// callback スレッド専用の待ち行列に積んだ (寝ていれば起こす)。
    fn wake_master(&self);
    /// グラフが終わった (待っている callback スレッドへ知らせる)。
    fn finished(&self);
    /// 仕事が積まれるかグラフが終わるまで寝る。寝る前に「寝る」ことを登録してから `graph` を見直す
    /// (起こし損ね防止)。`false` = 待ちの期限切れ (callback スレッドの bounded な待ち)。
    fn park(&self, graph: &RenderGraph) -> bool;
}

/// 積まれている job が無くなるまで取って `run` する (Ardour `Graph::run_one` と同型)。終えた job の後続のうち
/// 1 つはそのまま続けて実行し、残りは積んでその数だけ寝ている runner を起こす。
pub fn drain(graph: &RenderGraph, park: &impl Park, run: &mut impl FnMut(u32)) {
    let master = park.is_master();
    while let Some(mut job) = graph.pop(master) {
        loop {
            run(job);
            let f = graph.finish(job, master);
            if f.pushed > 0 {
                park.wake(f.pushed);
            }
            if f.for_master {
                park.wake_master();
            }
            if graph.is_done() {
                park.finished();
            }
            match f.next {
                Some(n) => job = n,
                None => break,
            }
        }
    }
}

/// グラフが終わるまで job を取って `run` する (callback スレッド / 1 スレッドの直列実行)。`false` = 期限切れ。
pub fn run_graph(graph: &RenderGraph, park: &impl Park, mut run: impl FnMut(u32)) -> bool {
    loop {
        drain(graph, park, &mut run);
        if graph.is_done() {
            return true;
        }
        if !park.park(graph) {
            return false;
        }
    }
}

#[cfg(test)]
mod tests;
