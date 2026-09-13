//! PR3: Plugin Delay Compensation.
//!
//! 各 track の **path latency** を計算し ([`path_latencies`])、 Mix の合流点で path 間の
//! 不一致を `ApplyDelay` で補償する ([`insert_delay_compensation`])。 master **出力**の
//! 遅延量 ([`master_output_latency`]) も同じ会計から出す。 Ardour の `Latent` 基底クラス +
//! `route.cc::process_output_buffers` の流儀:
//!
//!   path_latency(leaf)  = leaf の chain_latency
//!   path_latency(group) = max(child.path_latency) + group の chain_latency
//!
//! `chain_latency` は device 単位の報告値 (`device_latencies`) を track の
//! device chain 上で合計したもの。 GUI は集計せず device 単位で送ってくる
//! (r.md #9: 集計値を `Song` に持たせると保存されて「開いただけで `*`」)。
//!
//! ※ group では子達が group bus に流れ込むときに既に sibling alignment が
//!   行われている (ここで挿入する `ApplyDelay` で揃う) ので、 group の
//!   入力 bus 時点では全員 `max(child.path_latency)` に揃っている。
//!   従って group 自身の path_latency = max + own。
//!
//! 補償ルール: 各 Mix ノードの srcs の中で `path_latency` が最も小さい側に、
//!   `frames = max_path - this_path` の `ApplyDelay` を **Mix の直前に**
//!   挿入する。 こうすると合流点で全 src の累積 latency が揃う。

use common::model::Song;

use super::DeviceLatencies;
use super::deps::Topology;
use super::sidechain::{TapCtx, sidechain_input_latency};
use crate::graph::delay_line::DelayLine;
use crate::graph::program_build::program_latency;
use crate::graph::schedule::{BufRef, DelayKey, NodeOp};

/// 全 track の path latency (song-track index 順)。依存辺の fan-in
/// ([`compute_path_latency`]) の後に、パラアウト独立 dest の fan-in を重ねる。
pub(super) fn path_latencies(
    song: &Song,
    topo: &Topology,
    taps: &TapCtx<'_>,
    track_chain_latency: &[u32],
) -> Vec<u32> {
    let n = song.tracks.len();
    let mut path_latency = vec![u32::MAX; n];
    for i in 0..n {
        compute_path_latency(i as u32, song, track_chain_latency, topo, taps, &mut path_latency);
    }
    fan_in_paraout_latency(song, topo, track_chain_latency, &mut path_latency);
    path_latency
}

/// track `idx` の「サイドチェイン以外の入力」の latency = `max(group_input, send_input)`
/// (子の合流と send の合流が揃う位置)。pass 2 の consumer 宛ての `BusScAlign` の基準。
pub(super) fn non_sc_input_latencies(song: &Song, topo: &Topology, path_latency: &[u32]) -> Vec<u32> {
    let path = |i: u32| path_latency.get(i as usize).copied().unwrap_or(0);
    song.tracks
        .iter()
        .map(|t| {
            let group = topo.children_of.get(&t.id).map_or(0, |kids| kids.iter().map(|&c| path(c)).max().unwrap_or(0));
            let send = topo
                .incoming_sends
                .get(&t.id)
                .map_or(0, |edges| edges.iter().map(|&(s, _, _)| path(s)).max().unwrap_or(0));
            group.max(send)
        })
        .collect()
}

/// `path_latency[idx]` を計算してキャッシュする。 既に値があれば即返却
/// (memoization)。
///
/// PR3: group (`is_group` メンバ) は子の path_latency の最大値を自身の input
/// bus latency として、 そこに自身の chain latency (device 報告値の合計) を足す。
///
/// PR4 sidechain × PDC: track の単一 `devices` チェーン上の各 plugin の
/// `aux_inputs` の tap も `input bus latency` に取り込む
/// ([`sidechain_input_latency`])。 すなわち:
///
///   input_latency(T) = max(
///       max(child.path_latency for child in children_of(T)),
///       max(source.path_latency for (P, source) in sidechain_inputs(T))
///   )
///   path_latency(T) = input_latency(T) + T の chain latency
///
/// 仕様根拠: Ardour `route.cc::process_output_buffers` の "feed-forward
/// latency reporting" — sidechain edge も `Latent` の input の一種として
/// 扱う。 こうすると master / group bus の sibling alignment compensation が
/// sidechain 経由の遅延を含めた最大 path を基準に補償する (= 「サイドチェイン
/// 受信 track の出力が他の sibling track と musical time で揃う」)。
///
/// **注意 (本実装の限界)**: ここでは plugin の **入力 bus 単位** で latency
/// を揃える。 plugin が chain の途中 (slot K, K>0) にいるときは pre-plugin
/// chain prefix latency があり、 plugin 内部での main vs aux alignment は
/// それを考慮した DelayTrackInput op (= 別 PR) で完成する。 現状でも graph
/// layer の不変量は成立するので master / group の audio mix は崩れない。
///
/// dangling reference (= sidechain source が song に存在しない) は wrap せず
/// 0 として扱う (compile error にしない方針、 編集中の中間状態を許容)。
/// §5 (arch refactor) / r.md #129 §8.3.3: sidechain edge の実効 latency には、その consumer が
/// **pass 1** で走るとき `buffer_frames` (tap staging→消費の 1-buffer 遅延) が加算される。
/// pass 2 (bus の `ProcessGroupFx`) は同 buffer 内消費なので 0 ([`TapCtx::lag`])。
fn compute_path_latency(
    idx: u32,
    song: &Song,
    // `song.tracks` と同順の「その track の device chain が報告する latency の合計」。
    track_chain_latency: &[u32],
    topo: &Topology,
    taps: &TapCtx<'_>,
    cache: &mut [u32],
) -> u32 {
    if cache[idx as usize] != u32::MAX {
        return cache[idx as usize];
    }
    let track = &song.tracks[idx as usize];
    // 依存先 track の path latency (未計算なら再帰で求めて `cache` に入れる)。
    let path_of =
        |i: u32, cache: &mut [u32]| compute_path_latency(i, song, track_chain_latency, topo, taps, cache);

    let group_input: u32 = if topo.is_group.contains(&track.id) {
        topo.children_of
            .get(&track.id)
            .map(|kids| kids.iter().map(|&c| path_of(c, cache)).max().unwrap_or(0))
            .unwrap_or(0)
    } else {
        0
    };

    let sidechain_input = sidechain_input_latency(idx, track, taps, track_chain_latency, cache, path_of);

    // Aux-send fan-in: a return / bus depends on every track that sends
    // into it, so its input latency must also cover those sources. This
    // keeps the wet return time-aligned with the dry signal at the master
    // mix (the source's post-fader latency is carried by the send copy).
    let mut send_input: u32 = 0;
    if let Some(edges) = topo.incoming_sends.get(&track.id) {
        for &(src_idx, _, _) in edges {
            let l = path_of(src_idx, cache);
            if l > send_input {
                send_input = l;
            }
        }
    }

    let max_input = group_input.max(sidechain_input).max(send_input);
    let total = max_input.saturating_add(track_chain_latency[idx as usize]);
    cache[idx as usize] = total;
    total
}

/// パラアウト独立 dest の PDC fan-in (docs/plan_paraout.md): plugin の aux
/// 出力を「自分の子でない」 track へ振った独立トポロジでは、 dest の path
/// latency に source の path latency を取り込む (sidechain / send と同じ。
/// dest の入力 = source の aux なので、 source が遅れる分 dest も遅れる)。
/// 子 dest (group-with-instrument の子) は group fan-in 済み + 循環になるので
/// 除外する。 `reported` は既に path_latency[dest] に含まれるので
/// `max(existing, source_latency + dest.reported)` で更新 (= max(a,b)+c の
/// 分配律)。 健全な (非循環) paraout chain は深さ <= n で必ず収束するので
/// bounded fixpoint で回す。 相互 paraout (A.aux→D かつ D.aux→A — ParallelOutTap
/// は dep edge を張らないので既存の cycle 検出を通り抜ける病的ケース) でも n 回で
/// 打ち切り、 path_latency の発散 (= 際限ない DelayLine 確保 / ハング) を防ぐ。
fn fan_in_paraout_latency(
    song: &Song,
    topo: &Topology,
    track_chain_latency: &[u32],
    path_latency: &mut [u32],
) {
    for _ in 0..song.tracks.len() {
        let mut changed = false;
        for (dest_id, edges) in &topo.incoming_paraout {
            let Some(&d_idx) = topo.id_to_idx.get(dest_id) else {
                continue;
            };
            let dest = &song.tracks[d_idx as usize];
            for &(src_id, _, _) in edges {
                if dest.parent_group_id == Some(src_id) {
                    continue; // 子 dest は group fan-in 済み (循環回避)
                }
                let Some(&s_idx) = topo.id_to_idx.get(&src_id) else {
                    continue;
                };
                let cand = path_latency[s_idx as usize]
                    .saturating_add(track_chain_latency[d_idx as usize]);
                if cand > path_latency[d_idx as usize] {
                    path_latency[d_idx as usize] = cand;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// [`insert_delay_compensation`] の結果 (`Schedule` の同名 field へそのまま入る)。
pub(super) struct Compensated {
    pub(super) nodes: Vec<NodeOp>,
    pub(super) delay_lines: Vec<DelayLine>,
    /// `delay_lines` と平行な stable key (§5 D 状態移送用)。
    pub(super) delay_keys: Vec<DelayKey>,
    /// master 合流点で全 src が揃う latency (r.md #39)。master fx chain 自身の
    /// latency は含まない ([`master_output_latency`] が足す)。
    pub(super) master_mix_latency: u32,
}

/// 既存の nodes を線形に走査し、 Mix / MixAdditive を見つけたらその直前に
/// `ApplyDelay` を挿入する。 in-place 操作よりも build-from-scratch
/// の方が境界条件が単純なので、 一度別 Vec に組み直す。
/// §5 D: 各 DelayLine には stable key (`DelayKey`) を平行 Vec で持たせ、
/// 再 compile 時に `Schedule::adopt_state_from` が ring の内容を移送する。
///
/// r.md #129 §8.3.3: `ProcessGroupFx` の直前には、pass 2 の consumer が読むサイドチェインに bus の
/// 入力を揃える `ApplyDelay(BusScAlign)` を積む (`bus_sc_delay`、song-track index 順)。
/// `run_group_fx_chain` の pre-FX snapshot はその後に取られるので、自トラック Pre-FX の SC も揃う。
pub(super) fn insert_delay_compensation(
    song: &Song,
    nodes: Vec<NodeOp>,
    path_latency: &[u32],
    bus_sc_delay: &[u32],
) -> Compensated {
    let mut delay_lines: Vec<DelayLine> = Vec::new();
    let mut delay_keys: Vec<DelayKey> = Vec::new();
    let track_ids: Vec<u32> = song.tracks.iter().map(|t| t.id).collect();
    let mut out: Vec<NodeOp> = Vec::with_capacity(nodes.len() + song.tracks.len());
    let mut master_mix_latency: u32 = 0;
    for op in nodes.into_iter() {
        match &op {
            // clearing Mix: 全 src を最大 path latency に揃える。
            NodeOp::Mix { srcs, dst } => {
                let max_path = emit_mix_src_alignment(
                    srcs,
                    path_latency,
                    &track_ids,
                    &mut delay_lines,
                    &mut delay_keys,
                    &mut out,
                );
                if *dst == BufRef::Master {
                    master_mix_latency = max_path;
                }
            }
            // パラアウト MixAdditive (docs/plan_paraout.md): 子 (srcs) を揃える
            // のに加え、 dst (= group-with-instrument 自身の scratch にある prefix
            // main、 相対 latency 0) も子の最大 path latency 分だけ遅らせて揃える。
            // これをしないと、 子に latency 持ちプラグインがあるときキック (main)
            // とスネア等 (子経由) がサンプルずれる。 子の ApplyDelay と dst の
            // ApplyDelay を MixAdditive の直前に積むので、 加算時には両者が揃う。
            NodeOp::MixAdditive {
                srcs,
                dst: BufRef::TrackScratch(a_idx),
            } => {
                let max_path = emit_mix_src_alignment(
                    srcs,
                    path_latency,
                    &track_ids,
                    &mut delay_lines,
                    &mut delay_keys,
                    &mut out,
                );
                if max_path > 0 {
                    let track_id = track_ids.get(*a_idx as usize).copied().unwrap_or(0);
                    let buf = BufRef::TrackScratch(*a_idx);
                    let key = DelayKey::MixDst { track_id };
                    push_delay(buf, max_path, key, &mut delay_lines, &mut delay_keys, &mut out);
                }
            }
            NodeOp::MixAdditive { .. } => {
                // MixAdditive の dst は compile が常に TrackScratch で emit する。
            }
            NodeOp::ProcessGroupFx { track_idx, .. } => {
                let frames = bus_sc_delay.get(*track_idx as usize).copied().unwrap_or(0);
                if frames > 0 {
                    let track_id = track_ids.get(*track_idx as usize).copied().unwrap_or(0);
                    let buf = BufRef::TrackScratch(*track_idx);
                    let key = DelayKey::BusScAlign { track_id };
                    push_delay(buf, frames, key, &mut delay_lines, &mut delay_keys, &mut out);
                }
            }
            _ => {}
        }
        out.push(op);
    }
    Compensated {
        nodes: out,
        delay_lines,
        delay_keys,
        master_mix_latency,
    }
}

/// PDC helper: emit an `ApplyDelay` for every `TrackScratch` src whose path
/// latency is below the mix's max, so all srcs line up at the mix point.
/// Returns the max path latency over the srcs — `MixAdditive` uses it to also
/// align the dst's own pre-existing signal (the パラアウト instrument main,
/// `docs/plan_paraout.md`). Shared by the `Mix` and `MixAdditive` arms of the
/// PDC pass so the two stay in lock-step. `track_ids` は delay line の
/// stable key (`DelayKey::MixSrc`) 用の song-track-index → `Track::id` 表。
fn emit_mix_src_alignment(
    srcs: &[(BufRef, f32)],
    path_latency: &[u32],
    track_ids: &[u32],
    delay_lines: &mut Vec<DelayLine>,
    delay_keys: &mut Vec<DelayKey>,
    out: &mut Vec<NodeOp>,
) -> u32 {
    let max_path = srcs
        .iter()
        .filter_map(|(b, _)| match b {
            BufRef::TrackScratch(i) => Some(path_latency[*i as usize]),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    for (b, _) in srcs.iter() {
        let BufRef::TrackScratch(i) = b else {
            continue;
        };
        let this = path_latency[*i as usize];
        if this < max_path {
            let key = DelayKey::MixSrc {
                track_id: track_ids.get(*i as usize).copied().unwrap_or(0),
            };
            push_delay(BufRef::TrackScratch(*i), max_path - this, key, delay_lines, delay_keys, out);
        }
    }
    max_path
}

/// `buf` を `frames` だけ遅らせる `ApplyDelay` を `out` に積み、専用の delay line と
/// その状態移送キー `key` を平行 Vec に確保する。
fn push_delay(
    buf: BufRef,
    frames: u32,
    key: DelayKey,
    delay_lines: &mut Vec<DelayLine>,
    delay_keys: &mut Vec<DelayKey>,
    out: &mut Vec<NodeOp>,
) {
    let line_idx = delay_lines.len() as u32;
    // DelayLine.step は `delay <= capacity - 1` を要求
    // (`delay_line.rs` の clamp ロジック)。 補償量ちょうどを
    // 返すために capacity = frames + 1。
    delay_lines.push(DelayLine::with_capacity((frames as usize) + 1));
    delay_keys.push(key);
    out.push(NodeOp::ApplyDelay { buf, line_idx, frames });
}

/// master **出力**の遅延量 (`Schedule::master_latency_samples`)。
///
/// = master 合流点の path latency + master fx chain の報告 latency +
/// マスターリミッターのルックアヘッド (遅延を焼いたときだけ)。click の参照位置と書き出しの
/// 窓ずらしはどちらもこの 1 値を引くので、遅延源を足すときは必ずここに足す (r.md #39)。
///
/// `limiter_latency` = `Schedule::master_limiter_latency` (= `Song::master_limiter_latency_active`:
/// 静的 ON または On レーン / 変調)。DSP (`MasterLimiterState::process`) も同じ値で遅延を通すかを
/// 決めるので、On をオートメーションしても会計と実際の遅延が食い違わない (r.md #129 §18-G)。
///
/// `master_chain` = compile した master の fx chain (scope が master を通さないなら空)。
pub(super) fn master_output_latency(
    master_chain: &[common::model::Device],
    device_latencies: &DeviceLatencies,
    master_mix_latency: u32,
    sample_rate: u32,
    limiter_latency: bool,
    scope: common::protocol::RenderScope,
) -> u32 {
    let limiter = if limiter_latency { common::model::limiter_lookahead_samples(sample_rate) } else { 0 };
    master_mix_latency
        .saturating_add(program_latency(master_chain, device_latencies, scope))
        .saturating_add(limiter)
}
