//! クロスモジュレーションの依存グラフと 1 制御刻みの評価 (r.md #89)。
//!
//! 設計正本は [`docs/plan_rmd_88_89_cross_modulation.md`](../../docs/plan_rmd_88_89_cross_modulation.md)。
//!
//! **なぜ位相アキュムレータが要るか**: 瞬時周波数は瞬時位相の時間微分なので、位相は
//! 周波数の積分でしか得られない。rate を変調しながら `frac(rate(t)·t)` を評価すると
//! rate が変わった瞬間に過去の全区間へ遡って新 rate が適用され、位相が跳ぶ。
//! 参照実装も同じで、Vital `synth_lfo.cpp` は全 sync_type で無条件に積分し、
//! Surge `LFOModulationSource.cpp` は閉形式を locate 時のシードにしか使っていない。
//!
//! **未変調なら閉形式と厳密に一致する** ([`crate::modulators::cycle_pos`])。Sync は
//! `Σ dbeat/period = beat/period`、Free は `Σ dt·hz = secs·hz` と telescoping するので、
//! rate を変調していない既存曲の音は 1 サンプルも変わらない。
//!
//! **輪 (A→B→A) は繋げる** (r.md #89 Q2)。DFS の back-edge を 1 制御刻み遅延にして開く
//! (Bitwig の The Grid が「フィードバックは最小 1 ブロック遅延で開かせる」のと同じ考え方)。
//! 刻みが **絶対 song サンプル位置に整列**しているので、遅延量は buffer 長に依らず一定 =
//! 再生と書き出しが一致する。

use crate::automation::{mod_param_plain, mod_param_range, mod_param_norm};
use crate::model::{
    AutomationTarget, MASTER_TRACK_ID, ModParam, ModRate, ModRateMode, ModSource,
    ModSourceKind, Polarity, RetriggerMode, Song,
};
use crate::modulators::{GenParams, ModTime, cycle_pos, eval_generator};

/// 制御グリッドの刻み幅 (サンプル)。既存 automation のサブバッファ刻みと同じ。
/// **絶対 song サンプル位置に整列**するので buffer 境界に依存しない。
pub const MOD_TICK_FRAMES: u32 = 64;

/// [`ModPhaseTable`] の breakpoint 間隔 (刻み)。breakpoint が必ず刻みに乗るので、
/// breakpoint からの前進が曲頭からの通しと **厳密に一致**する (近似ではない)。
pub const MOD_PHASE_BREAKPOINT_TICKS: i64 = 512;

/// 表を張る最大長 (秒)。`tempo_map.rs` の `MAX_TABLE_BEATS` と同じ理由の hard cap —
/// 破損 / 悪意ある project の巨大な `length_beats` で `Vec::with_capacity` が
/// OOM / panic するのを防ぐ。超える曲では表を張らず [`ModTier::Audio`] に倒す。
pub const MOD_PHASE_TABLE_MAX_SECS: f64 = 24.0 * 3600.0;

/// 変調できる param の数 ([`ModParam::ALL`] と同じ)。
pub const MOD_PARAM_COUNT: usize = 10;

/// `build_plan` の作業用: 1 本の入力辺 (変調元の位置, param, 深さ, 極性, `ModRouting::id`)。
type RawEdge = (usize, ModParam, f32, Polarity, u32);
/// `adj[dst]` = dst に入ってくる辺。
type Adjacency = [Vec<RawEdge>];
/// 深さが動く変調の作業用 (`ModRouting::id`, 深さを動かす辺, lane があるか)。
type RawDepth = (u32, Vec<(usize, f32, Polarity)>, bool);

/// このソースの位相が何に依存するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModTier {
    /// rate が変調されていない。閉形式 O(1)。今日と bit 一致。
    Closed,
    /// rate が変調されているが鎖に follower を含まない。曲位置だけで決まるので
    /// [`ModPhaseTable`] で「どこから再生しても同じ位相」を保てる。
    Integrated,
    /// rate の鎖に follower が居る。音に依存するので事前計算できず、
    /// **再生を始めた位置で位相が変わる** (r.md #89 Q7 = 許す。ラックに印を出す)。
    Audio,
}

/// 1 本の入力辺 (source → この node の param)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModEdge {
    pub param: ModParam,
    /// 変調元の slot ([`ModPlan::nodes`] 内の位置)。
    pub src_slot: u16,
    /// この辺の **静止した**深さ。深さ自体が変調 / オートメーションされている辺は
    /// `depth_group` が `Some` になり、刻みごとの実効値がそちらに入る。
    pub depth: f32,
    pub polarity: Polarity,
    /// 輪を開くために **1 刻み前の値**を読む辺 (DFS の back-edge)。
    pub delayed: bool,
    /// 深さが動く辺なら [`ModPlan::depth_groups`] の添字 (r.md #89 Q9)。
    pub depth_group: Option<u16>,
}

/// **深さ自体が動く変調 1 本** (r.md #89 Q9 = Bitwig の modulation scaling)。
///
/// `ModRouting` の深さは、`AutomationTarget::ModRoutingDepth { routing_id }` を
/// 指す変調とオートメーションレーンで動かせる。動く深さは
/// [`ModRuntime::depth_for`] が刻みごとに解決し、変調先が
/// モジュレーターのツマミなら [`tick`] が、track / plugin param なら
/// [`crate::automation::modulation_offset_norm`] 系がそれを使う。
#[derive(Debug, Clone, PartialEq)]
pub struct DepthGroup {
    /// 深さが動く対象の `ModRouting::id`。
    pub routing_id: u32,
    /// モデル上の深さ (レーンが無ければこれが base)。
    pub base_depth: f32,
    /// この深さを動かす変調の辺。加算スタック (`modulation_offset_norm` と同契約)。
    pub edges: Vec<DepthEdge>,
    /// `ModRoutingDepth` の automation lane があるか (engine が刻みごとに
    /// [`ModRuntime::set_depth_base`] へ書く)。
    pub has_lane: bool,
}

/// [`DepthGroup`] に入る 1 本の辺。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthEdge {
    pub src_slot: u16,
    pub depth: f32,
    pub polarity: Polarity,
}

/// トポロジカル順に並んだ 1 ソース。
#[derive(Debug, Clone, PartialEq)]
pub struct ModNode {
    pub source_id: u32,
    /// off-RT で clone する。RT では clone しない (`Vec` を持つので heap 確保になる)。
    pub kind: ModSourceKind,
    pub tier: ModTier,
    pub in_edges: Vec<ModEdge>,
    /// 輪に属するか (ラックの ⟳ バッジ)。
    pub in_cycle: bool,
    pub rate: Option<ModRate>,
    pub retrigger: RetriggerMode,
    /// `FromBeat` の anchor を秒へ換算した値 (Free の retrigger 用、r.md #88)。
    pub anchor_secs: f64,
    /// 各 param の **変調前の値** (plain)。`Rate` だけは tempo に依存するので
    /// `rate` から毎刻み求める (ここには入れない)。
    pub base: [f64; MOD_PARAM_COUNT],
}

/// off-RT で `Song` から作る評価計画。RT は読むだけ。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModPlan {
    /// トポロジカル順 (輪の back-edge は切ってある)。
    pub nodes: Vec<ModNode>,
    /// slot → `ModSource::id`。値面と GUI 表示を **id で**結ぶ (不変条件 1)。
    pub slot_ids: Vec<u32>,
    /// slot_ids と値面を跨いで読む瞬間のレースを塞ぐ世代。
    pub generation: u64,
    /// automation lane が base を上書きする param の一覧 (engine が刻みごとに解決して
    /// [`ModRuntime::set_base`] へ書く)。`(slot, param)`。
    pub lane_params: Vec<(u16, ModParam)>,
    /// **深さが動く変調** (r.md #89 Q9)。添字が [`ModEdge::depth_group`] /
    /// [`ModRuntime::depth_for`] の鍵。
    pub depth_groups: Vec<DepthGroup>,
}

impl ModPlan {
    /// `source_id` の slot。
    #[must_use]
    pub fn slot_of(&self, source_id: u32) -> Option<u16> {
        self.slot_ids
            .iter()
            .position(|id| *id == source_id)
            .and_then(|i| u16::try_from(i).ok())
    }

    /// 位相の積分が要る (= 表 / シードが要る) ソースが 1 つでもあるか。
    #[must_use]
    pub fn needs_integration(&self) -> bool {
        self.nodes.iter().any(|n| n.tier != ModTier::Closed)
    }

    /// `routing_id` の深さが動くなら [`Self::depth_groups`] の添字。
    #[must_use]
    pub fn depth_group_of(&self, routing_id: u32) -> Option<u16> {
        self.depth_groups
            .iter()
            .position(|g| g.routing_id == routing_id)
            .and_then(|i| u16::try_from(i).ok())
    }
}

// =====================================================================
// plan の構築 (off-RT)
// =====================================================================

/// `Song` からトポロジカル順の評価計画を作る。**輪の判定・tier 判定の唯一の口**で、
/// GUI (⟳ / 位置依存バッジ) と engine と export がこの 1 本を引く
/// (`AutomationTarget::accepts_launcher_cells` と同じ「片側だけで弾かない」規約)。
///
/// `anchor_secs` は `FromBeat` の anchor を秒へ換算する関数 (テンポマップが要るので
/// 呼び出し側が渡す。`crate::automation::beats_to_samples` / sample_rate で作れる)。
pub fn build_plan(
    song: &Song,
    generation: u64,
    anchor_secs: impl Fn(f64) -> f64,
) -> ModPlan {
    let n = song.mod_sources.len();
    // 1. 辺を集める。id → 元の位置。
    let pos_of = |id: u32| song.mod_sources.iter().position(|m| m.id == id);
    // adj[dst] = 入ってくる (src_pos, param, depth, polarity)
    let mut adj: Vec<Vec<RawEdge>> = vec![Vec::new(); n];
    for r in song.all_mod_routings() {
        let AutomationTarget::ModSourceParam { source_id, param } = &r.target else {
            continue;
        };
        let (Some(dst), Some(src)) = (pos_of(*source_id), pos_of(r.source_id)) else {
            continue;
        };
        // 種別に存在しない param は評価から外す (種別を戻せば復活するので消しはしない)。
        if !param.exists_on(&song.mod_sources[dst].kind) {
            continue;
        }
        adj[dst].push((src, *param, r.depth, r.polarity, r.id));
    }

    // 1.5. r.md #89 Q9: **深さが動く変調**を集める。
    //
    // 深さは (a) `ModRoutingDepth { routing_id }` を指す変調と (b) 同 target の
    // automation lane で動く。ここで拾わないと routing も lane も保存されるのに
    // 深さは永久に静止したまま = 設計正本が禁じている「保存はされるのに効かない」。
    let mut depth_src: Vec<RawDepth> = Vec::new();
    fn depth_slot(
        v: &mut Vec<RawDepth>,
        rid: u32,
    ) -> usize {
        match v.iter().position(|(id, ..)| *id == rid) {
            Some(i) => i,
            None => {
                v.push((rid, Vec::new(), false));
                v.len() - 1
            }
        }
    }
    for r in song.all_mod_routings() {
        if let AutomationTarget::ModRoutingDepth { routing_id } = &r.target
            && let Some(src) = pos_of(r.source_id)
        {
            let i = depth_slot(&mut depth_src, *routing_id);
            depth_src[i].1.push((src, r.depth, r.polarity));
        }
    }
    // lane 側 (置き場は対象 routing と同じ)。
    for rid in song.all_mod_routings().map(|r| r.id).collect::<Vec<_>>() {
        let target = AutomationTarget::ModRoutingDepth { routing_id: rid };
        let owner = song.mod_routing_owner(rid).unwrap_or(MASTER_TRACK_ID);
        if song_has_lane(song, owner, &target) {
            let i = depth_slot(&mut depth_src, rid);
            depth_src[i].2 = true;
        }
    }
    let base_depth_of = |rid: u32| {
        song.all_mod_routings().find(|r| r.id == rid).map_or(0.0, |r| r.depth)
    };

    // 2. DFS でトポロジカル順を作り、back-edge を delayed に落とす。
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut state = vec![0u8; n]; // 0=未訪問 1=訪問中 2=完了
    let mut back_edges: Vec<(usize, usize)> = Vec::new(); // (dst, src)
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        // 再帰せずに明示スタックで回す (深い鎖でスタックを溢れさせない)。
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state[start] = 1;
        while let Some((node, edge_i)) = stack.pop() {
            if edge_i < adj[node].len() {
                stack.push((node, edge_i + 1));
                let src = adj[node][edge_i].0;
                match state[src] {
                    0 => {
                        state[src] = 1;
                        stack.push((src, 0));
                    }
                    // 訪問中 = 自分の祖先 → back-edge (輪)。
                    1 => back_edges.push((node, src)),
                    _ => {}
                }
            } else {
                state[node] = 2;
                order.push(node);
            }
        }
    }

    // 3. 輪に属するノードを塗る。
    //
    // back-edge `(dst, src)` は「`src` が `dst` を変調し、かつ `dst` から辿ると
    // `src` に戻る」= 輪 `dst ⟶ … ⟶ src ⟶ dst` を意味する。輪に載るのは
    // **dst から流れが届き、かつ src へ流れが届く**ノード。
    //
    // `adj` は **入力辺** (`adj[x]` = x を変調するノード) なので向きが逆になる:
    // 「dst から v へ流れる」= `reaches(v, dst)`、「v から src へ流れる」=
    // `v` が `src` の上流 = `reaches(src, v)` ではなく src から adj を辿って v に届くこと。
    let mut in_cycle = vec![false; n];
    for &(dst, src) in &back_edges {
        // src の上流 (= src へ流れ込む全ノード)。dst も必ず含む。
        let mut upstream_of_src = vec![false; n];
        upstream_of_src[src] = true;
        let mut stack = vec![src];
        while let Some(v) = stack.pop() {
            for &(u, ..) in &adj[v] {
                if !upstream_of_src[u] {
                    upstream_of_src[u] = true;
                    stack.push(u);
                }
            }
        }
        for (v, &up) in upstream_of_src.iter().enumerate() {
            if up && reaches(&adj, v, dst) {
                in_cycle[v] = true;
            }
        }
    }

    // 4. slot 割当 (order = トポロジカル順)。
    let mut slot_of_pos = vec![0u16; n];
    for (slot, &pos) in order.iter().enumerate() {
        slot_of_pos[pos] = u16::try_from(slot).unwrap_or(u16::MAX);
    }

    // 5. tier 判定。
    //
    // 位相の積分が要るのは **rate が動くとき**だけ。動かす経路は 2 つある:
    // 変調の辺 (`ModParam::Rate` に入る辺) と、`ModParam::Rate` の automation lane。
    // lane を数えないと「レーンで速さを描いたのに閉形式で評価される」= 位相が跳ぶ。
    let rate_modulated: Vec<bool> = (0..n)
        .map(|i| {
            adj[i].iter().any(|(_, p, ..)| *p == ModParam::Rate)
                || song_has_lane(
                    song,
                    song.mod_sources[i].owner_track_id,
                    &AutomationTarget::ModSourceParam {
                        source_id: song.mod_sources[i].id,
                        param: ModParam::Rate,
                    },
                )
        })
        .collect();
    // `audio_dep[x]` = x の **出力**が音に依存するか (どの param 経由でも伝播する)。
    let audio_dep = audio_dependency(song, &adj);
    // テンポそのものが音で動くなら、刻みの `dt_beats` が音依存になる。
    // 位相表はフォロワーを 0 として焼くので、この曲では **どのソースも**表で
    // 再現できない (レビュー確定: サイドチェイン → SongTempo の構成で seek のたびに
    // 全 Integrated の位相が飛ぶのに「位置依存」バッジが出なかった)。
    let tempo_is_audio_driven = song.song_mod_routings.iter().any(|r| {
        r.target == AutomationTarget::SongTempo
            && pos_of(r.source_id).is_some_and(|p| audio_dep[p])
    });
    // **位相**が音に依存するのは「rate に入る辺の上流に follower が居る」ときだけ。
    // 位相と無関係な param (φ / 幅 / なめらかさ) に follower を挿しただけで
    // 位置依存へ落とすと、表で厳密一致できるはずの構成が seek で飛ぶ。
    let rate_chain_is_audio = |i: usize| {
        tempo_is_audio_driven
            || adj[i]
                .iter()
                .any(|&(s, p, ..)| p == ModParam::Rate && audio_dep[s])
    };

    let mut nodes = Vec::with_capacity(n);
    let mut slot_ids = Vec::with_capacity(n);
    let mut lane_params = Vec::new();
    for (slot, &pos) in order.iter().enumerate() {
        let src: &ModSource = &song.mod_sources[pos];
        let slot_u16 = u16::try_from(slot).unwrap_or(u16::MAX);
        let tier = if !rate_modulated[pos] {
            ModTier::Closed
        } else if rate_chain_is_audio(pos) {
            ModTier::Audio
        } else {
            ModTier::Integrated
        };
        let retrigger = src.kind.retrigger().unwrap_or(RetriggerMode::FreeRun);
        let anchor = match retrigger {
            RetriggerMode::FromBeat { anchor_beat } => anchor_secs(anchor_beat),
            RetriggerMode::FreeRun => 0.0,
        };
        let in_edges = adj[pos]
            .iter()
            .map(|&(s, param, depth, polarity, routing_id)| ModEdge {
                param,
                src_slot: slot_of_pos[s],
                depth,
                polarity,
                delayed: back_edges.contains(&(pos, s)),
                depth_group: depth_src
                    .iter()
                    .position(|(rid, ..)| *rid == routing_id)
                    .and_then(|i| u16::try_from(i).ok()),
            })
            .collect();
        // lane 上書きの対象を集める (engine が刻みごとに解決する)。
        for param in ModParam::ALL {
            if !param.exists_on(&src.kind) {
                continue;
            }
            let target = AutomationTarget::ModSourceParam { source_id: src.id, param };
            if song_has_lane(song, src.owner_track_id, &target) {
                lane_params.push((slot_u16, param));
            }
        }
        nodes.push(ModNode {
            source_id: src.id,
            kind: src.kind.clone(),
            tier,
            in_edges,
            in_cycle: in_cycle[pos],
            rate: src.kind.rate(),
            retrigger,
            anchor_secs: anchor,
            base: base_params(&src.kind),
        });
        slot_ids.push(src.id);
    }
    // 深さの群は slot 割当のあとで組む (辺の src を slot で持つため)。
    let depth_groups = depth_src
        .into_iter()
        .map(|(routing_id, edges, has_lane)| DepthGroup {
            routing_id,
            base_depth: base_depth_of(routing_id),
            edges: edges
                .into_iter()
                .map(|(s, depth, polarity)| DepthEdge {
                    src_slot: slot_of_pos[s],
                    depth,
                    polarity,
                })
                .collect(),
            has_lane,
        })
        .collect();
    ModPlan { nodes, slot_ids, generation, lane_params, depth_groups }
}

/// `from` から辺を辿って `to` に到達できるか (輪の塗り分け用)。
fn reaches(adj: &Adjacency, from: usize, to: usize) -> bool {
    let mut seen = vec![false; adj.len()];
    let mut stack = vec![from];
    while let Some(v) = stack.pop() {
        if v == to {
            return true;
        }
        for &(u, ..) in &adj[v] {
            if !seen[u] {
                seen[u] = true;
                stack.push(u);
            }
        }
    }
    false
}

/// 各ソースが「audio に依存する鎖」に載っているか (follower から到達可能か)。
fn audio_dependency(
    song: &Song,
    adj: &Adjacency,
) -> Vec<bool> {
    let n = song.mod_sources.len();
    let mut dep: Vec<bool> = song
        .mod_sources
        .iter()
        .map(|m| matches!(m.kind, ModSourceKind::EnvelopeFollower { .. }))
        .collect();
    // 到達可能性の固定点 (n 回で必ず収束)。
    for _ in 0..n {
        let mut changed = false;
        for i in 0..n {
            if dep[i] {
                continue;
            }
            if adj[i].iter().any(|&(s, ..)| dep[s]) {
                dep[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    dep
}

/// `owner_track_id` の置き場に `target` の automation lane があるか。
fn song_has_lane(song: &Song, owner_track_id: u32, target: &AutomationTarget) -> bool {
    if owner_track_id == 0 || owner_track_id == MASTER_TRACK_ID {
        return song.song_lanes.iter().any(|l| &l.target == target);
    }
    song.tracks
        .iter()
        .find(|t| t.id == owner_track_id)
        .is_some_and(|t| t.automation_lanes.iter().any(|l| &l.target == target))
}

/// **`ModParam` の「今の値」(plain) を読む唯一の口。** ラックのツマミ / オートメーション
/// レーンの既定値 / 変調の base が全部これを引く (SSoT)。
///
/// `Rate` だけは tempo に依存する ([`ModRate::base_hz`]) ので `bpm` が要る。
/// その param を持たない種別は `0.0`。
#[must_use]
pub fn param_plain(kind: &ModSourceKind, param: ModParam, bpm: f64) -> f64 {
    use crate::model::LfoShape;
    match (param, kind) {
        (ModParam::Rate, _) => kind.rate().map_or(0.0, |r| r.base_hz(bpm)),
        (ModParam::LfoPhase, ModSourceKind::Lfo(c)) => f64::from(c.phase),
        (ModParam::LfoPulseWidth, ModSourceKind::Lfo(c)) => match c.shape {
            LfoShape::Pulse { width } => f64::from(width),
            _ => 0.5,
        },
        (ModParam::RandomSmooth, ModSourceKind::Random(c)) => f64::from(c.smooth),
        (ModParam::StepsSlew, ModSourceKind::Steps(c)) => f64::from(c.slew),
        (ModParam::FollowerAttack, ModSourceKind::EnvelopeFollower { follower, .. }) => {
            f64::from(follower.attack_ms)
        }
        (ModParam::FollowerRelease, ModSourceKind::EnvelopeFollower { follower, .. }) => {
            f64::from(follower.release_ms)
        }
        (ModParam::FollowerGain, ModSourceKind::EnvelopeFollower { follower, .. }) => {
            f64::from(follower.gain)
        }
        (ModParam::FollowerHpHz, ModSourceKind::EnvelopeFollower { follower, .. }) => {
            follower
                .band_filter
                .map_or(f64::from(crate::model::MOD_BAND_HZ_MIN), |b| f64::from(b.hp_hz))
        }
        (ModParam::FollowerLpHz, ModSourceKind::EnvelopeFollower { follower, .. }) => {
            follower
                .band_filter
                .map_or(f64::from(crate::model::MOD_BAND_HZ_MAX), |b| f64::from(b.lp_hz))
        }
        _ => 0.0,
    }
}

/// config から各 param の変調前 plain 値を取り出す (`Rate` は tempo 依存なので 0)。
fn base_params(kind: &ModSourceKind) -> [f64; MOD_PARAM_COUNT] {
    let mut b = [0.0; MOD_PARAM_COUNT];
    for param in ModParam::ALL {
        if param != ModParam::Rate {
            b[param.index()] = param_plain(kind, param, 0.0);
        }
    }
    b
}

// =====================================================================
// RT ランタイム
// =====================================================================

/// 1 刻みぶんの時刻。`dt_beats` / `dt_secs` は **この刻みで進む量**で、
/// 呼び出し側 (engine / export / 表の構築) が同じ式で出す。
#[derive(Debug, Clone, Copy, Default)]
pub struct TickCtx {
    pub beat: f64,
    pub secs: f64,
    pub bpm: f64,
    pub dt_beats: f64,
    pub dt_secs: f64,
    /// 絶対 song サンプル位置 / [`MOD_TICK_FRAMES`]。
    pub tick_index: i64,
}

/// RT 側の状態。**事前確保のみ**で、`tick` は alloc / lock / I/O をしない。
#[derive(Debug, Clone, Default)]
pub struct ModRuntime {
    phase: Vec<f64>,
    value: Vec<f32>,
    prev: Vec<f32>,
    /// envelope follower の出力 (engine ring が `set_follower` で書く)。
    follower: Vec<f32>,
    base: Vec<[f64; MOD_PARAM_COUNT]>,
    /// その param の base を **automation lane が上書きしたか** ([`ModPlan::lane_params`])。
    /// `Rate` の base は tempo 依存で `ModRate::base_hz` から毎刻み求めるので、
    /// 「レーンが書いたのか、まだ誰も書いていない 0.0 なのか」を値では区別できない。
    /// `install` で false に戻り、[`ModRuntime::set_base`] で立つ。
    base_from_lane: Vec<[bool; MOD_PARAM_COUNT]>,
    /// 各ソースの実効パラメータ (フォロワー係数の再計算に engine が使う)。
    eff: Vec<[f64; MOD_PARAM_COUNT]>,
    /// 深さが動く変調の **base** (`ModPlan::depth_groups` と同じ並び)。
    /// レーンがあれば engine が刻みごとに [`Self::set_depth_base`] で上書きする。
    depth_base: Vec<f32>,
    /// 深さが動く変調の **実効値** (同上)。[`tick`] が刻みの先頭で解決する。
    depth: Vec<f32>,
    /// 次に来るはずの刻み。ずれたら位相を張り直す (locate / ループ折返し)。
    next_tick: i64,
    generation: u64,
}

impl ModRuntime {
    /// plan に合わせて確保し直す (off-RT。plan 差し替え時に呼ぶ)。
    pub fn install(&mut self, plan: &ModPlan) {
        let n = plan.nodes.len();
        self.phase.clear();
        self.phase.resize(n, 0.0);
        self.value.clear();
        self.value.resize(n, 0.0);
        self.prev.clear();
        self.prev.resize(n, 0.0);
        self.follower.clear();
        self.follower.resize(n, 0.0);
        self.base.clear();
        self.base.extend(plan.nodes.iter().map(|node| node.base));
        self.base_from_lane.clear();
        self.base_from_lane.resize(n, [false; MOD_PARAM_COUNT]);
        self.depth_base.clear();
        self.depth_base
            .extend(plan.depth_groups.iter().map(|g| g.base_depth));
        self.depth.clear();
        self.depth
            .extend(plan.depth_groups.iter().map(|g| g.base_depth));
        self.eff.clear();
        self.eff.resize(n, [0.0; MOD_PARAM_COUNT]);
        self.generation = plan.generation;
        // 次の tick で必ず張り直させる。
        self.next_tick = i64::MIN;
    }

    /// automation lane で base を上書きする ([`ModPlan::lane_params`] のぶんだけ)。
    pub fn set_base(&mut self, slot: u16, param: ModParam, plain: f64) {
        if let Some(row) = self.base.get_mut(usize::from(slot)) {
            row[param.index()] = plain;
        }
        if let Some(row) = self.base_from_lane.get_mut(usize::from(slot)) {
            row[param.index()] = true;
        }
    }

    /// `ModRoutingDepth` の automation lane が書いた深さ (r.md #89 Q9)。
    /// `plan.depth_groups` に居ない `routing_id` は no-op。
    pub fn set_depth_base(&mut self, plan: &ModPlan, routing_id: u32, plain: f32) {
        if let Some(i) = plan.depth_group_of(routing_id)
            && let Some(v) = self.depth_base.get_mut(usize::from(i))
        {
            *v = plain.clamp(-1.0, 1.0);
        }
    }

    /// `routing_id` の **刻み時点の実効深さ**。深さが動かない変調は `None`
    /// (呼び出し側は `ModRouting::depth` をそのまま使う)。
    #[must_use]
    pub fn depth_for(&self, plan: &ModPlan, routing_id: u32) -> Option<f32> {
        let i = plan.depth_group_of(routing_id)?;
        self.depth.get(usize::from(i)).copied()
    }

    /// 直近の刻みの出力 (unipolar 0..=1)。
    #[must_use]
    pub fn value(&self, slot: u16) -> f32 {
        self.value.get(usize::from(slot)).copied().unwrap_or(0.0)
    }

    /// `ModSource::id` 引きの出力 (`ModRouting::source_id` を解決する唯一の口)。
    #[must_use]
    pub fn value_by_id(&self, plan: &ModPlan, source_id: u32) -> f32 {
        plan.slot_of(source_id).map_or(0.0, |s| self.value(s))
    }

    /// 実効パラメータ (フォロワーの係数再計算に使う)。
    #[must_use]
    pub fn effective(&self, slot: u16, param: ModParam) -> f64 {
        self.eff
            .get(usize::from(slot))
            .map_or(0.0, |row| row[param.index()])
    }

    /// 積分中のソースの位相 (cycles)。GUI のカーソルはこれを読む (自前計算しない)。
    #[must_use]
    pub fn phase(&self, slot: u16) -> f64 {
        self.phase.get(usize::from(slot)).copied().unwrap_or(0.0)
    }

    /// envelope follower の出力を書き込む (engine ring が算出した `env`)。
    ///
    /// **`slot` は [`ModPlan`] の slot** — `Song::mod_sources` の位置ではない
    /// (plan はトポロジカル順に並べ替える)。engine は `plan.slot_of(source_id)` で
    /// 解決してから呼ぶこと。次の [`tick`] がこの値をそのままそのソースの出力にする。
    pub fn set_follower(&mut self, slot: u16, env: f32) {
        if let Some(v) = self.follower.get_mut(usize::from(slot)) {
            *v = env.clamp(0.0, 1.0);
        }
    }
}

/// 1 制御刻みを進める。**クロス変調の評価点はここ 1 つ**。
///
/// envelope follower の出力は事前に [`ModRuntime::set_follower`] で書いておく
/// (引数で渡さないのは、plan の slot 順と `Song::mod_sources` の順が違うため —
/// 呼び出し側に `plan.slot_of(id)` を通させて取り違えを構造的に防ぐ)。
/// `table` があれば locate 時の位相を厳密に張り直す ([`ModTier::Integrated`])。
///
/// RT 安全: alloc / lock / I/O 無し。`ModSourceKind` を clone しない。
pub fn tick(
    plan: &ModPlan,
    rt: &mut ModRuntime,
    table: Option<&ModPhaseTable>,
    ctx: TickCtx,
) {
    if rt.value.len() != plan.nodes.len() {
        // install 漏れ (plan 差し替え直後)。RT では確保できないので何もしない。
        return;
    }
    if ctx.tick_index != rt.next_tick {
        seed_phases(plan, rt, table, ctx);
    }
    // r.md #89 Q9: **深さの実効値**を刻みの先頭で解決する。
    //
    // 深さを動かすソースは、その深さが効く辺の下流にも居られる (深さの輪)。
    // だから深さは **1 刻み前の値** (`rt.prev`) から作る — 辺の評価順に依存せず、
    // 輪の 1 刻み遅延と同じ規則なので刻みが絶対位置に整列している限り決定論的。
    for (i, g) in plan.depth_groups.iter().enumerate() {
        let mut d = rt.depth_base[i];
        for e in &g.edges {
            let s = rt.prev[usize::from(e.src_slot)].clamp(0.0, 1.0);
            d += match e.polarity {
                Polarity::Unipolar => e.depth * s,
                Polarity::Bipolar => e.depth * (2.0 * s - 1.0),
            };
        }
        rt.depth[i] = d.clamp(-1.0, 1.0);
    }
    for (slot, node) in plan.nodes.iter().enumerate() {
        // --- 1. 入力辺を畳んで実効パラメータを出す ---
        let mut off = [0.0f32; MOD_PARAM_COUNT];
        for e in &node.in_edges {
            let s = if e.delayed {
                rt.prev[usize::from(e.src_slot)]
            } else {
                rt.value[usize::from(e.src_slot)]
            }
            .clamp(0.0, 1.0);
            // 深さが動く辺は刻みの実効値を使う (r.md #89 Q9)。
            let depth = match e.depth_group {
                Some(i) => rt.depth[usize::from(i)],
                None => e.depth,
            };
            off[e.param.index()] += match e.polarity {
                Polarity::Unipolar => depth * s,
                Polarity::Bipolar => depth * (2.0 * s - 1.0),
            };
        }
        let base = rt.base[slot];
        let mut eff = [0.0f64; MOD_PARAM_COUNT];
        for param in ModParam::ALL {
            let i = param.index();
            if param == ModParam::Rate {
                continue; // 下で tempo と一緒に解く。
            }
            eff[i] = apply_offset(param, base[i], off[i]);
        }

        // --- 2. 位相 / 周期位置 ---
        // `Rate` の base は tempo 依存なので毎刻み `base_hz` から求める。ただし
        // **automation lane が書いていればそちらが base** (レーンの plain 単位は Hz)。
        // これを見ないと `ModPlan::lane_params` が Rate を集めても評価に効かない。
        let base_hz = if rt.base_from_lane[slot][ModParam::Rate.index()] {
            rt.base[slot][ModParam::Rate.index()]
        } else {
            node.rate.map_or(0.0, |r| r.base_hz(ctx.bpm))
        };
        let hz_eff = if off[ModParam::Rate.index()] == 0.0 {
            base_hz
        } else {
            apply_offset(ModParam::Rate, base_hz, off[ModParam::Rate.index()])
        };
        eff[ModParam::Rate.index()] = hz_eff;

        let cp = match (node.rate, node.tier) {
            (Some(rate), ModTier::Closed) => cycle_pos(
                &rate,
                ModTime { beat: ctx.beat, secs: ctx.secs, anchor_secs: node.anchor_secs },
                &node.retrigger,
            ),
            (Some(rate), _) => {
                // 位相は瞬時周波数の積分。未変調なら閉形式と telescoping で一致する。
                let mult = if base_hz > 0.0 { hz_eff / base_hz } else { 1.0 };
                let dphi_base = match rate.mode {
                    ModRateMode::Sync => ctx.dt_beats / rate.period_beats(),
                    ModRateMode::Free => ctx.dt_secs * base_hz,
                };
                let p = rt.phase[slot];
                rt.phase[slot] = p + dphi_base * mult;
                p
            }
            (None, _) => 0.0,
        };

        // --- 3. 出力 ---
        let v = match &node.kind {
            ModSourceKind::EnvelopeFollower { .. } => {
                rt.follower[slot]
            }
            kind => {
                let g = GenParams {
                    cycle_pos: cp,
                    lfo_phase: eff[ModParam::LfoPhase.index()] as f32,
                    pulse_width: eff[ModParam::LfoPulseWidth.index()] as f32,
                    random_smooth: eff[ModParam::RandomSmooth.index()] as f32,
                    steps_slew: eff[ModParam::StepsSlew.index()] as f32,
                };
                eval_generator(kind, g).unwrap_or(0.0)
            }
        };
        rt.value[slot] = v;
        rt.eff[slot] = eff;
    }
    rt.prev.copy_from_slice(&rt.value);
    rt.next_tick = ctx.tick_index + 1;
}

/// 正規化領域で `off` を足してから plain へ戻す (値域の SSoT は `mod_param_range`)。
#[inline]
fn apply_offset(param: ModParam, base: f64, off: f32) -> f64 {
    if off == 0.0 {
        return base;
    }
    match mod_param_range(param) {
        Some(_) => {
            let n = (mod_param_norm(param, base) + f64::from(off)).clamp(0.0, 1.0);
            mod_param_plain(param, n)
        }
        None => (base + f64::from(off)).clamp(0.0, 1.0),
    }
}

/// locate / ループ折返しで位相を張り直す。
///
/// - [`ModTier::Integrated`] は表があれば breakpoint から厳密に前進、無ければ閉形式で近似。
/// - [`ModTier::Audio`] は閉形式でシードする (Surge の `attackFrom` と同じ)。
///   音に依存する鎖なので「再生を始めた位置で位相が変わる」— これは仕様 (r.md #89 Q7)。
fn seed_phases(plan: &ModPlan, rt: &mut ModRuntime, table: Option<&ModPhaseTable>, ctx: TickCtx) {
    for (slot, node) in plan.nodes.iter().enumerate() {
        if node.tier == ModTier::Closed {
            continue;
        }
        let Some(rate) = node.rate else { continue };
        let from_table = if node.tier == ModTier::Integrated {
            table.and_then(|t| t.phase_at(slot, ctx.tick_index))
        } else {
            None
        };
        rt.phase[slot] = from_table.unwrap_or_else(|| {
            cycle_pos(
                &rate,
                ModTime { beat: ctx.beat, secs: ctx.secs, anchor_secs: node.anchor_secs },
                &node.retrigger,
            )
        });
    }
}


/// **刻みごとの transport を進める規則の SSoT。**
///
/// engine / export / [`ModPhaseTable::build`] / [`locate`] が全部この 1 本を踏む。
/// 踏まないと表と実演奏の位相がずれる (浮動小数の丸めまで一致させる必要がある)。
///
/// - `secs` は刻み番号からの **積** (`tick * dt_secs`)。engine の
///   `playhead / sample_rate` と厳密に一致する (playhead は刻みの倍数)。
/// - `beat` は **累算**。テンポオートメーション + テンポ変調に追従させるため。
/// - `bpm` は次の刻みのテンポ (`SongTempo` カーブ + その変調。変調は 1 刻み前の値)。
#[must_use]
pub fn next_mark(
    song: &Song,
    plan: &ModPlan,
    rt: &ModRuntime,
    mark: PhaseMark,
    tick_index: i64,
    dt_secs: f64,
) -> PhaseMark {
    let beat = mark.beat + dt_secs * mark.bpm / 60.0;
    let base = f64::from(crate::automation::evaluate_song_tempo(song, beat));
    let bpm = crate::automation::apply_modulation(
        &AutomationTarget::SongTempo,
        base,
        &song.song_mod_routings,
        |id| rt.value_by_id(plan, id),
    )
    .max(1.0);
    PhaseMark { beat, secs: (tick_index + 1) as f64 * dt_secs, bpm }
}

/// `from_tick`..`to_tick` を [`next_mark`] の規則で回す共有ループ。
/// `on_breakpoint` は breakpoint の刻みで、その刻みを評価する **前**に呼ばれる。
fn walk(
    plan: &ModPlan,
    rt: &mut ModRuntime,
    song: &Song,
    dt_secs: f64,
    mut mark: PhaseMark,
    ticks: std::ops::Range<i64>,
    mut on_breakpoint: impl FnMut(&ModRuntime, PhaseMark),
) -> PhaseMark {
    for k in ticks {
        if k % MOD_PHASE_BREAKPOINT_TICKS == 0 {
            on_breakpoint(rt, mark);
        }
        let dt_beats = dt_secs * mark.bpm / 60.0;
        // follower は音に依存するので replay では 0 にする — **表もそう焼いている**。
        // ライブ走行中の生 env を引きずると、locate の replay が表と別のテンポ
        // (`SongTempo` の変調経由) を踏んで、breakpoint からの前進が表と一致しなくなる。
        // Audio tier はこのあと閉形式でシードし直すので影響しない。
        rt.follower.fill(0.0);
        tick(
            plan,
            rt,
            None,
            TickCtx {
                beat: mark.beat,
                secs: mark.secs,
                bpm: mark.bpm,
                dt_beats,
                dt_secs,
                tick_index: k,
            },
        );
        mark = next_mark(song, plan, rt, mark, k, dt_secs);
    }
    mark
}

/// **locate (シーク / ループ折返し / 再生開始) で位相を張り直す唯一の口。**
///
/// [`ModTier::Integrated`] は表の breakpoint から **同じ漸化式で** `target_tick` まで
/// 再生し直す。表の構築と同一の刻み・順序・f64 加算を踏むので、曲頭から通しで再生した
/// ときと **厳密に一致**する (「どこから再生しても同じ位相」が近似でなく成立する)。
/// 前進は高々 [`MOD_PHASE_BREAKPOINT_TICKS`] 刻みで有界。
///
/// 表が無い / 範囲外 / 積分が不要なら、次の [`tick`] が閉形式でシードするよう印だけ付ける。
pub fn locate(
    plan: &ModPlan,
    rt: &mut ModRuntime,
    table: Option<&ModPhaseTable>,
    song: &Song,
    sample_rate: u32,
    target_tick: i64,
) {
    if !plan.needs_integration() || rt.value.len() != plan.nodes.len() {
        rt.next_tick = i64::MIN;
        return;
    }
    let Some(table) = table.filter(|t| t.slots() == plan.nodes.len()) else {
        rt.next_tick = i64::MIN;
        return;
    };
    let Some((b, mark)) = table.mark_at(target_tick) else {
        rt.next_tick = i64::MIN;
        return;
    };
    for (slot, node) in plan.nodes.iter().enumerate() {
        if node.tier == ModTier::Integrated
            && let Some(p) = table.phase_at(slot, target_tick)
        {
            rt.phase[slot] = p;
        }
    }
    // 輪 (`ModEdge::delayed`) は「1 刻み前の値」を読む。位相だけ戻して replay を
    // 始めると、replay 1 刻み目の遅延辺が 0 か前回再生の残骸を読み、そこで入った
    // 誤差が積分器に残って輪で増幅される。位相と同じ格子で焼いた値を一緒に戻す。
    if let Some(prev) = table.prev_at(target_tick)
        && prev.len() == rt.prev.len()
    {
        rt.prev.copy_from_slice(prev);
    }
    let dt_secs = f64::from(MOD_TICK_FRAMES) / f64::from(sample_rate.max(1));
    let start = b as i64 * MOD_PHASE_BREAKPOINT_TICKS;
    rt.next_tick = start;
    let end = walk(plan, rt, song, dt_secs, mark, start..target_tick, |_, _| {});
    let (beat, secs) = (end.beat, end.secs);
    // Audio tier は replay で再現できない (音に依存する) ので閉形式でシードし直す。
    for (slot, node) in plan.nodes.iter().enumerate() {
        if node.tier == ModTier::Audio
            && let Some(rate) = node.rate
        {
            rt.phase[slot] = cycle_pos(
                &rate,
                ModTime { beat, secs, anchor_secs: node.anchor_secs },
                &node.retrigger,
            );
        }
    }
    rt.next_tick = target_tick;
}

// =====================================================================
// ModPhaseTable — 位置依存にしないための位相表
// =====================================================================

/// [`ModTier::Integrated`] のソースの位相を **breakpoint ごとに**焼いた表。
/// `tempo_map.rs` と同型 (off-thread build / lookup は alloc・lock 無し)。
///
/// breakpoint が必ず制御刻みに乗る ([`MOD_PHASE_BREAKPOINT_TICKS`] の倍数) ので、
/// breakpoint から前進した和は曲頭からの和と **厳密に一致**する。よって
/// 「どこから再生しても同じ位相」が近似でなく成立する。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModPhaseTable {
    /// `phases[slot][b]` = breakpoint b (= tick `b * MOD_PHASE_BREAKPOINT_TICKS`) の位相。
    /// `Closed` / `Audio` の slot は空 `Vec`。
    phases: Vec<Vec<f64>>,
    /// breakpoint 時点の **1 刻み前の出力** (= その刻みの [`ModRuntime::prev`])。
    ///
    /// 輪 (`ModEdge::delayed`) は `rt.prev` を読むので、位相だけ戻して replay を
    /// 始めると **replay 1 刻み目の遅延辺が 0 か前回再生の残骸を読む**。そこで入った
    /// 誤差は積分器に残り、輪で増幅されて成長する (実測: 1/4 sync・depth 0.2 の
    /// 相互 rate 変調で tick 4000 のとき 0.015 周期)。位相と同じ格子で焼いて
    /// [`locate`] が一緒に戻すことで、通し再生と厳密に一致させる。
    ///
    /// `[breakpoint][slot]` の並び (slot 数は plan 全体ぶん — 遅延辺の src は
    /// `Closed` tier のこともあるので Integrated だけでは足りない)。
    prevs: Vec<Vec<f32>>,
    /// breakpoint での transport 状態 (再現 walk の起点)。`phases` と同じ長さ。
    marks: Vec<PhaseMark>,
    pub generation: u64,
}

/// breakpoint 時点の transport 状態。ここから同じ漸化式で前進すれば
/// 曲頭からの通しと **厳密に一致**する。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PhaseMark {
    pub beat: f64,
    pub secs: f64,
    pub bpm: f64,
}

impl ModPhaseTable {
    /// **off-thread で**曲頭から刻みループを再生して表を張る。
    ///
    /// engine と **同じ刻み・同じ順序・同じ f64 加算**を踏むので、breakpoint から
    /// 前進した位相は曲頭からの通しと厳密に一致する。テンポは engine と同じく
    /// `SongTempo` カーブ + その変調で毎刻み解く (変調は前の刻みの値を使う —
    /// engine の 1 刻み lag と同じ規則)。
    ///
    /// [`ModTier::Audio`] の slot は音に依存するので表を張らない (空 `Vec`)。
    /// 積分が要る slot が無い / 曲が [`MOD_PHASE_TABLE_MAX_SECS`] を超える場合は空表。
    #[must_use]
    pub fn build(plan: &ModPlan, song: &Song, sample_rate: u32, length_secs: f64) -> Self {
        let n = plan.nodes.len();
        let mut phases: Vec<Vec<f64>> = vec![Vec::new(); n];
        let mut prevs: Vec<Vec<f32>> = Vec::new();
        let mut marks: Vec<PhaseMark> = Vec::new();
        let integrated: Vec<bool> =
            plan.nodes.iter().map(|x| x.tier == ModTier::Integrated).collect();
        if sample_rate == 0
            || !integrated.iter().any(|b| *b)
            || !length_secs.is_finite()
            || length_secs <= 0.0
            || length_secs > MOD_PHASE_TABLE_MAX_SECS
        {
            return Self { phases, prevs, marks, generation: plan.generation };
        }
        let sr = f64::from(sample_rate);
        let dt_secs = f64::from(MOD_TICK_FRAMES) / sr;
        let total_ticks = (length_secs / dt_secs).ceil() as i64 + 1;
        let n_breakpoints = usize::try_from(total_ticks / MOD_PHASE_BREAKPOINT_TICKS + 1)
            .unwrap_or(0);
        for (slot, on) in integrated.iter().enumerate() {
            if *on {
                phases[slot] = Vec::with_capacity(n_breakpoints);
            }
        }
        prevs.reserve(n_breakpoints);
        let mut rt = ModRuntime::default();
        rt.install(plan);
        let mark0 = PhaseMark {
            beat: 0.0,
            secs: 0.0,
            bpm: f64::from(crate::automation::evaluate_song_tempo(song, 0.0)).max(1.0),
        };
        walk(plan, &mut rt, song, dt_secs, mark0, 0..total_ticks, |rt, mark| {
            for (slot, on) in integrated.iter().enumerate() {
                if *on {
                    phases[slot].push(rt.phase(u16::try_from(slot).unwrap_or(u16::MAX)));
                }
            }
            // 遅延辺が読む「1 刻み前の値」。この callback は刻み `k` を評価する **前**に
            // 呼ばれるので、`rt.prev` はちょうど刻み `k-1` の出力になっている。
            prevs.push(rt.prev.clone());
            marks.push(mark);
        });
        Self { phases, prevs, marks, generation: plan.generation }
    }

    /// `tick_index` 以下の直近 breakpoint の index と transport 状態。
    #[must_use]
    pub fn mark_at(&self, tick_index: i64) -> Option<(usize, PhaseMark)> {
        if tick_index < 0 {
            return None;
        }
        let b = usize::try_from(tick_index / MOD_PHASE_BREAKPOINT_TICKS).ok()?;
        self.marks.get(b).map(|m| (b, *m))
    }

    /// `tick_index` 以下の直近 breakpoint の位相。表の範囲外は `None`
    /// (= 呼び出し側が閉形式シードに倒す)。
    ///
    /// **breakpoint そのものの値**を返す。刻みの端数ぶんの前進は engine が
    /// 通常の `tick` で進める (同じ格子点を踏むので厳密一致)。
    #[must_use]
    pub fn phase_at(&self, slot: usize, tick_index: i64) -> Option<f64> {
        if tick_index < 0 {
            return None;
        }
        let b = usize::try_from(tick_index / MOD_PHASE_BREAKPOINT_TICKS).ok()?;
        self.phases.get(slot)?.get(b).copied()
    }

    /// 表を張った時点の slot 数 (plan と食い違ったら使わない)。
    #[must_use]
    pub fn slots(&self) -> usize {
        self.phases.len()
    }

    /// `tick_index` 以下の直近 breakpoint 時点の「1 刻み前の出力」(遅延辺が読む値)。
    #[must_use]
    pub fn prev_at(&self, tick_index: i64) -> Option<&[f32]> {
        if tick_index < 0 {
            return None;
        }
        let b = usize::try_from(tick_index / MOD_PHASE_BREAKPOINT_TICKS).ok()?;
        self.prevs.get(b).map(Vec::as_slice)
    }

    /// 表が張られている slot か。
    #[must_use]
    pub fn covers(&self, slot: usize) -> bool {
        self.phases.get(slot).is_some_and(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LfoConfig, LfoShape, ModRouting, ModSourceKind, Track};

    fn lfo_source(id: u32, rate: ModRate) -> ModSource {
        ModSource {
            id,
            owner_track_id: 1,
            color: [0.0; 3],
            kind: ModSourceKind::Lfo(LfoConfig {
                shape: LfoShape::SawUp,
                rate,
                phase: 0.0,
                retrigger: RetriggerMode::FreeRun,
            }),
        }
    }

    fn song_with(sources: Vec<ModSource>, routings: Vec<ModRouting>) -> Song {
        Song {
            tracks: vec![Track { id: 1, mod_routings: routings, ..Default::default() }],
            mod_sources: sources,
            ..Default::default()
        }
    }

    fn quarter() -> ModRate {
        ModRate::default()
    }

    fn ctx_at(tick: i64, bpm: f64, sr: f64) -> TickCtx {
        let dt_secs = f64::from(MOD_TICK_FRAMES) / sr;
        let dt_beats = dt_secs * bpm / 60.0;
        TickCtx {
            beat: tick as f64 * dt_beats,
            secs: tick as f64 * dt_secs,
            bpm,
            dt_beats,
            dt_secs,
            tick_index: tick,
        }
    }

    /// 設計正本 §8-1: **未変調の rate は閉形式と bit 一致する**。
    /// 既存曲の音を 1 サンプルも変えないことの担保。
    #[test]
    fn 未変調のrateは閉形式とbit一致する() {
        for rate in [
            quarter(),
            ModRate { mode: ModRateMode::Free, hz: 3.0, ..ModRate::default() },
        ] {
            let song = song_with(vec![lfo_source(1, rate)], vec![]);
            let plan = build_plan(&song, 1, |_| 0.0);
            assert_eq!(plan.nodes[0].tier, ModTier::Closed, "未変調は Closed tier");
            let mut rt = ModRuntime::default();
            rt.install(&plan);
            for k in [0i64, 1, 17, 1000] {
                let ctx = ctx_at(k, 120.0, 48_000.0);
                tick(&plan, &mut rt, None, ctx);
                let closed = crate::modulators::generator_scalar(
                    &plan.nodes[0].kind,
                    ModTime::new(ctx.beat, ctx.secs),
                )
                .unwrap();
                assert_eq!(rt.value(0), closed, "rate={rate:?} tick={k}");
            }
        }
    }

    /// 3 ノードの輪 (1→2→3→1) で **中間ノードにも** ⟳ 印が立ち、輪の外側
    /// (輪の下流にぶら下がっているだけ) には立たないこと。
    #[test]
    fn 輪の中間ノードにも印が立つ() {
        let edge = |id, from, to| ModRouting {
            id,
            target: AutomationTarget::ModSourceParam { source_id: to, param: ModParam::Rate },
            source_id: from,
            depth: 0.2,
            polarity: Polarity::Bipolar,
        };
        let song = song_with(
            vec![
                lfo_source(1, quarter()),
                lfo_source(2, quarter()),
                lfo_source(3, quarter()),
                // 輪の外側に 1 本ぶら下げる (こちらには印が立たないこと)。
                lfo_source(4, quarter()),
            ],
            vec![edge(1, 1, 2), edge(2, 2, 3), edge(3, 3, 1), edge(4, 3, 4)],
        );
        let plan = build_plan(&song, 1, |_| 0.0);
        for id in [1u32, 2, 3] {
            let slot = plan.slot_of(id).unwrap();
            assert!(plan.nodes[usize::from(slot)].in_cycle, "id={id} は輪の一部");
        }
        let outside = plan.slot_of(4).unwrap();
        assert!(
            !plan.nodes[usize::from(outside)].in_cycle,
            "輪の下流にぶら下がっているだけのノードには印を立てない"
        );
    }

    /// 設計正本 §8-3: 輪は back-edge が 1 刻み遅延で開き、両端に `in_cycle` が立つ。
    /// 同じ刻み列を 2 回回して bit 一致すること (決定論)。
    #[test]
    fn 輪はback_edgeが1刻み遅延で開き決定論的() {
        let song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter())],
            vec![
                ModRouting {
                    id: 1,
                    target: AutomationTarget::ModSourceParam {
                        source_id: 2,
                        param: ModParam::Rate,
                    },
                    source_id: 1,
                    depth: 0.2,
                    polarity: Polarity::Bipolar,
                },
                ModRouting {
                    id: 2,
                    target: AutomationTarget::ModSourceParam {
                        source_id: 1,
                        param: ModParam::Rate,
                    },
                    source_id: 2,
                    depth: 0.2,
                    polarity: Polarity::Bipolar,
                },
            ],
        );
        let plan = build_plan(&song, 1, |_| 0.0);
        assert!(plan.nodes.iter().all(|n| n.in_cycle), "輪の両端に印が立つ");
        assert_eq!(
            plan.nodes.iter().flat_map(|n| n.in_edges.iter()).filter(|e| e.delayed).count(),
            1,
            "輪は 1 箇所だけ遅延で開く"
        );
        let run = || {
            let mut rt = ModRuntime::default();
            rt.install(&plan);
            let mut out = Vec::new();
            for k in 0..64 {
                tick(&plan, &mut rt, None, ctx_at(k, 120.0, 48_000.0));
                out.push((rt.value(0), rt.value(1)));
            }
            out
        };
        assert_eq!(run(), run(), "同じ刻み列は bit 一致");
    }

    /// r.md #89 Q9: **変調 1 本の深さ**を別のモジュレーターで動かせること。
    ///
    /// 深さは plan 構築時の静止値ではなく刻みごとの実効値で効く。ここが繋がって
    /// いないと routing もレーンも保存されるのに深さは永久に動かない
    /// (設計正本が `accepts_launcher_cells` の教訓として禁じている
    /// 「保存はされるのに永久に効かない」)。
    #[test]
    fn 変調の深さを別のモジュレーターで動かせる() {
        // #1 が #2 の φ を深さ 0.5 で変調し、その **深さ** を #3 が動かす。
        let song = song_with(
            vec![
                lfo_source(1, quarter()),
                lfo_source(2, quarter()),
                lfo_source(3, quarter()),
            ],
            vec![
                ModRouting {
                    id: 10,
                    target: AutomationTarget::ModSourceParam {
                        source_id: 2,
                        param: ModParam::LfoPhase,
                    },
                    source_id: 1,
                    depth: 0.5,
                    polarity: Polarity::Unipolar,
                },
                ModRouting {
                    id: 11,
                    target: AutomationTarget::ModRoutingDepth { routing_id: 10 },
                    source_id: 3,
                    depth: -0.5,
                    polarity: Polarity::Unipolar,
                },
            ],
        );
        let plan = build_plan(&song, 1, |_| 0.0);
        assert_eq!(plan.depth_groups.len(), 1, "深さが動く変調を 1 本拾う");
        assert_eq!(plan.depth_groups[0].routing_id, 10);
        let slot2 = plan.slot_of(2).unwrap();
        let mut rt = ModRuntime::default();
        rt.install(&plan);
        // 実効深さが刻みごとに動く (= 0.5 に張り付かない)。
        let mut seen: Vec<f32> = Vec::new();
        for k in 0..64 {
            tick(&plan, &mut rt, None, ctx_at(k, 120.0, 48_000.0));
            seen.push(rt.depth_for(&plan, 10).unwrap());
        }
        assert!(
            seen.iter().any(|d| (*d - 0.5).abs() > 1e-6),
            "深さが静止したままなら Q9 は成立していない: {seen:?}"
        );
        // 深さが動いた結果、変調先の出力も静止値のときと違う値になる。
        let mut flat = song.clone();
        flat.tracks[0].mod_routings.retain(|r| r.id != 11);
        let flat_plan = build_plan(&flat, 1, |_| 0.0);
        let mut flat_rt = ModRuntime::default();
        flat_rt.install(&flat_plan);
        let flat_slot2 = flat_plan.slot_of(2).unwrap();
        let mut differed = false;
        for k in 0..64 {
            tick(&plan, &mut rt, None, ctx_at(k, 120.0, 48_000.0));
            tick(&flat_plan, &mut flat_rt, None, ctx_at(k, 120.0, 48_000.0));
            if (rt.value(slot2) - flat_rt.value(flat_slot2)).abs() > 1e-6 {
                differed = true;
            }
        }
        assert!(differed, "深さの変調が変調先の出力に届いていない");
    }

    /// tier は **位相が何に依存するか**だけで決まること。
    ///
    /// レビューで確定した 3 つの取り違えを固定する:
    /// - rate と無関係な param (φ) に follower を挿しただけで `Audio` に落ちる
    ///   (= 表で厳密一致できるはずの構成が seek で飛び、誤った「位置依存」バッジが出る)。
    /// - `ModParam::Rate` の automation lane を数えないので `Closed` のまま
    ///   (= レーンで速さを描くと閉形式評価に倒れて位相が跳ぶ)。
    /// - テンポ自体が音で動く曲を数えないので `Integrated` のまま
    ///   (= 表はフォロワー 0 で焼くので実演奏と食い違う)。
    #[test]
    fn tierは位相が何に依存するかだけで決まる() {
        let follower = |id: u32| ModSource {
            id,
            owner_track_id: 1,
            color: [0.0; 3],
            kind: ModSourceKind::EnvelopeFollower {
                tap: crate::model::AudioTap::post_fader(1),
                follower: crate::model::FollowerConfig::default(),
            },
        };
        let edge = |id: u32, from: u32, to: u32, param: ModParam| ModRouting {
            id,
            target: AutomationTarget::ModSourceParam { source_id: to, param },
            source_id: from,
            depth: 0.3,
            polarity: Polarity::Bipolar,
        };
        let tier_of = |song: &Song, id: u32| {
            let plan = build_plan(song, 1, |_| 0.0);
            let slot = plan.slot_of(id).unwrap();
            plan.nodes[usize::from(slot)].tier
        };

        // 1) rate を純 LFO が変調 + φ を follower が変調 → 位相の鎖は clean なので Integrated。
        let song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter()), follower(3)],
            vec![
                edge(1, 1, 2, ModParam::Rate),
                edge(2, 3, 2, ModParam::LfoPhase),
            ],
        );
        assert_eq!(tier_of(&song, 2), ModTier::Integrated, "φ の follower は位相に無関係");

        // 2) rate の鎖に follower が居る → Audio。
        let song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter()), follower(3)],
            vec![
                edge(1, 3, 1, ModParam::LfoPhase),
                edge(2, 1, 2, ModParam::Rate),
            ],
        );
        assert_eq!(tier_of(&song, 2), ModTier::Audio, "rate の上流に follower");

        // 3) 変調は無いが `Rate` の automation lane がある → Closed ではない。
        let mut song = song_with(vec![lfo_source(1, quarter())], vec![]);
        song.tracks[0].automation_lanes.push(crate::model::AutomationLane::new(
            AutomationTarget::ModSourceParam { source_id: 1, param: ModParam::Rate },
            2.0,
        ));
        assert_eq!(tier_of(&song, 1), ModTier::Integrated, "レーンで速さが動く");

        // 4) テンポ自体が音で動く → 表で再現できないので Audio。
        let mut song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter()), follower(3)],
            vec![edge(1, 1, 2, ModParam::Rate)],
        );
        song.song_mod_routings.push(ModRouting {
            id: 9,
            target: AutomationTarget::SongTempo,
            source_id: 3,
            depth: 0.2,
            polarity: Polarity::Unipolar,
        });
        assert_eq!(tier_of(&song, 2), ModTier::Audio, "テンポが音で動く曲は表を使えない");
    }

    /// **輪があっても** locate が通し再生と厳密に一致すること。
    ///
    /// 輪の遅延辺は `ModRuntime::prev` (1 刻み前の出力) を読む。位相だけ表から戻して
    /// replay を始めると replay 1 刻み目が 0 か前回再生の残骸を読み、その誤差が
    /// 積分器に残って輪で増幅される (レビュー確定: tick 4000 で 0.015 周期)。
    /// breakpoint ちょうど (replay 0 刻み) では露見しないので、**途中の刻み**で見る。
    #[test]
    fn 輪があってもlocateが通し再生と一致する() {
        let mk = |id: u32, from: u32, to: u32| ModRouting {
            id,
            target: AutomationTarget::ModSourceParam { source_id: to, param: ModParam::Rate },
            source_id: from,
            depth: 0.2,
            polarity: Polarity::Bipolar,
        };
        let song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter())],
            vec![mk(1, 1, 2), mk(2, 2, 1)],
        );
        let plan = build_plan(&song, 1, |_| 0.0);
        assert!(plan.nodes.iter().all(|n| n.in_cycle), "輪になっている");
        assert!(
            plan.nodes.iter().all(|n| n.tier == ModTier::Integrated),
            "follower が居ないので位置非依存であるべき"
        );
        let sr = 48_000u32;
        let dt_secs = f64::from(MOD_TICK_FRAMES) / f64::from(sr);
        let table = ModPhaseTable::build(&plan, &song, sr, dt_secs * 4096.0);
        let mark0 = PhaseMark {
            beat: 0.0,
            secs: 0.0,
            bpm: f64::from(crate::automation::evaluate_song_tempo(&song, 0.0)).max(1.0),
        };
        // breakpoint 直後 (1025) / 途中 (1300) / 遠く (4000) の 3 点で見る。
        for target in [1025i64, 1300, 4000] {
            let mut walked = ModRuntime::default();
            walked.install(&plan);
            walk(&plan, &mut walked, &song, dt_secs, mark0, 0..target, |_, _| {});
            let mut located = ModRuntime::default();
            located.install(&plan);
            locate(&plan, &mut located, Some(&table), &song, sr, target);
            for slot in [0u16, 1] {
                assert_eq!(
                    located.phase(slot),
                    walked.phase(slot),
                    "target={target} slot={slot}"
                );
            }
        }
    }

    /// 設計正本 §8-4: rate を変調しているソースの位相が、**表の breakpoint から前進した場合**と
    /// **曲頭から通しで再生した場合**で厳密に一致すること (= どこから再生しても同じ位相)。
    /// 近似ではなく bit 一致であることが「位置依存にしない」の担保。
    #[test]
    fn rate変調時の位相がbreakpointからの前進と通し再生で一致する() {
        let song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter())],
            vec![ModRouting {
                id: 1,
                target: AutomationTarget::ModSourceParam { source_id: 2, param: ModParam::Rate },
                source_id: 1,
                depth: 0.3,
                polarity: Polarity::Bipolar,
            }],
        );
        let plan = build_plan(&song, 1, |_| 0.0);
        let target_slot = plan.slot_of(2).unwrap();
        assert_eq!(
            plan.nodes[usize::from(target_slot)].tier,
            ModTier::Integrated,
            "rate を変調されたソースは積分 tier"
        );
        let sr = 48_000u32;
        let dt_secs = f64::from(MOD_TICK_FRAMES) / f64::from(sr);
        let table = ModPhaseTable::build(&plan, &song, sr, dt_secs * 4096.0);
        // 通し再生 (表の構築と同じ共有ループ `walk` を踏む)。
        const T: i64 = 1300; // breakpoint (512 の倍数) をまたぐ中途半端な位置
        let mut walked = ModRuntime::default();
        walked.install(&plan);
        let mark0 = PhaseMark {
            beat: 0.0,
            secs: 0.0,
            bpm: f64::from(crate::automation::evaluate_song_tempo(&song, 0.0)).max(1.0),
        };
        walk(&plan, &mut walked, &song, dt_secs, mark0, 0..T, |_, _| {});
        // breakpoint からの前進。
        let mut located = ModRuntime::default();
        located.install(&plan);
        locate(&plan, &mut located, Some(&table), &song, sr, T);
        assert_eq!(
            located.phase(target_slot),
            walked.phase(target_slot),
            "breakpoint からの前進と通しが bit 一致すること"
        );
    }

    /// 設計正本 §8-5: ソースを消すと自分を指す target も **連鎖して**消える。冪等。
    #[test]
    fn sourceを消すと自分を指すクロス変調も連鎖して消える() {
        let mut song = song_with(
            vec![lfo_source(1, quarter()), lfo_source(2, quarter())],
            vec![
                // #1 → #2 の rate
                ModRouting {
                    id: 1,
                    target: AutomationTarget::ModSourceParam {
                        source_id: 2,
                        param: ModParam::Rate,
                    },
                    source_id: 1,
                    depth: 0.2,
                    polarity: Polarity::Bipolar,
                },
                // #2 → 「#1 の変調の深さ」(routing_id=1)
                ModRouting {
                    id: 2,
                    target: AutomationTarget::ModRoutingDepth { routing_id: 1 },
                    source_id: 2,
                    depth: 0.5,
                    polarity: Polarity::Unipolar,
                },
            ],
        );
        // ソース #1 を消す → routing#1 が消える → それを指す routing#2 も消える。
        song.mod_sources.retain(|m| m.id != 1);
        assert!(song.prune_dangling_mod_targets(), "1 回目は変化する");
        assert!(song.tracks[0].mod_routings.is_empty(), "連鎖して全部消える");
        assert!(!song.prune_dangling_mod_targets(), "2 回目は変化しない (冪等)");
    }
}
