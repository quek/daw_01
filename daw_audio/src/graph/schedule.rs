//! Schedule + NodeOp + BufRef.
//!
//! A `Schedule` is the compiled execution plan that the audio thread
//! steps through every buffer. It owns its delay-line ring buffers and
//! its port-buffer pool so the RT path never allocates.
//!
//! v29 (`docs/plan_arch_refactor.md` §1): plugin を参照する op は
//! `(track, device_index)` の positional key ではなく **安定 device id**
//! (`PluginInstance::id`) を compile 時に焼き込む。engine はそれで
//! `plugin_refs` (device_id → shmem) を直接引く。

#![allow(dead_code)]

use super::delay_line::DelayLine;
use super::port_buffer::PortBufferPool;
use super::program::ChainProgram;

/// Reference to a stereo audio buffer.
///
/// `BufRef` is the only way `NodeOp` describes inputs and outputs; the
/// schedule executor resolves it to a concrete buffer (per-track scratch,
/// the master bus, the port pool, or a plugin's aux output port).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufRef {
    /// A track's post-fader scratch buffer (`mixer::TrackScratch`).
    /// Indexed by song-track index. PR1 only uses these.
    TrackScratch(u32),
    /// The master bus output.
    Master,
    /// A buffer drawn from `Schedule::port_buffers`. Used (PR2 onwards)
    /// for group-bus inputs that don't map to any track's own scratch.
    Pooled(u32),
    /// A track's **pre-fader** scratch (after its fx chain, before the
    /// volume / pan strip). Written by `ProcessTrack` / `ProcessGroupFx`
    /// and read by a `MixSend` whose send `mode == PreFader`. Indexed by
    /// song-track index, parallel to `TrackScratch`.
    PreFaderScratch(u32),
    /// A track's **pre-FX** scratch (the raw signal *before* its device
    /// chain — audio clips / sidechain-aligned input, with no FX applied).
    /// Captured by `ProcessTrack` / `ProcessGroupFx` just before the device
    /// loop runs, and read by a `SidechainTap` / `EnvelopeFollow` whose tap
    /// point is `TapPoint::PreFx`. Indexed by song-track index, parallel to
    /// `TrackScratch`. docs/plan_modulation_followups.md §1.
    PreFxScratch(u32),
    /// r.md #110 Parallel: `owner` (song-track index、master は [`MASTER_OWNER`]) の
    /// program の chain `slot` の **PostFx** snapshot (device 通過後・gain/pan 前)。
    ChainPostFx { owner: u32, slot: u32 },
    /// 同 chain の **PostFader** snapshot (gain/pan/mute 後)。
    ChainPostFader { owner: u32, slot: u32 },
    /// 同 program の Parallel `slot` の入力 (= その Parallel の全 chain の `PreFx`)。
    ParallelInput { owner: u32, slot: u32 },
    /// r.md #112: 同 program の Parallel `slot` の `Split` 出力 `output` (= その chain の `PreFx`)。
    ParallelOutput { owner: u32, slot: u32, output: u8 },
}

/// `BufRef::Chain* { owner }` / `ParallelInput { owner }` で master program を指す sentinel。
pub const MASTER_OWNER: u32 = u32::MAX;

/// A unit of work in a `Schedule`. The RT thread iterates `Schedule::nodes`
/// in order and dispatches each variant; the audio worker pool fans
/// `ProcessTrack` / `ProcessGroupFx` ops out across cores.
#[derive(Debug, Clone)]
pub enum NodeOp {
    /// Run the full per-track pipeline (sequencer → MIDI FX →
    /// instrument / vocal → audio FX → strip). Output lands in the
    /// track's scratch (`BufRef::TrackScratch(track_idx)`).
    ProcessTrack { track_idx: u32 },

    /// PR2: process a group / return / bus track's audio FX chain on its
    /// already-summed input scratch, then apply its strip.
    ///
    /// `start_op` = the first op index in the track's `ChainProgram` to run.
    /// `0` for a pure group / return (its whole chain is bus FX). For a
    /// **パラアウト group-with-instrument** (`docs/plan_paraout.md`) the
    /// instrument prefix `[0..pass1_end]` already ran in pass 1
    /// (`process_track_owned`, producing the main signal + aux outputs), so this
    /// op runs only the suffix FX `[start_op..]` on the summed bus (instrument
    /// main + children) — that's how "the instrument track's own FX process the
    /// whole kit" is realised. r.md #110: index は `Track.devices` ではなく展開後の
    /// op 列 (`ChainProgram::pass1_end`)。
    ProcessGroupFx { track_idx: u32, start_op: u32 },

    /// Mix `srcs` into `dst` with per-source linear gain (clearing `dst`
    /// first). PR1 emits a single `Mix { dst: Master, ... }` at the end; PR2
    /// also emits `Mix { dst: TrackScratch(group_idx), ... }` for group inputs.
    Mix {
        srcs: Vec<(BufRef, f32)>,
        dst: BufRef,
    },

    /// パラアウト (`docs/plan_paraout.md`): like `Mix` but **accumulates** into
    /// `dst` instead of clearing it. Used for a group-with-instrument track
    /// whose own instrument output is already sitting in its scratch (written
    /// by the pass-1 prefix): the children are summed *on top* of it before the
    /// suffix FX run. A clearing `Mix` would wipe the instrument's main signal.
    MixAdditive {
        srcs: Vec<(BufRef, f32)>,
        dst: BufRef,
    },

    /// PR3: apply a PDC delay line to `buf` in place using `delay_lines[line_idx]`
    /// for `frames` samples of read-out latency.
    ApplyDelay {
        buf: BufRef,
        line_idx: u32,
        frames: u32,
    },

    /// PR4 sidechain: copy `src` into the plugin `device_id`'s
    /// `aux_in_port` shmem buffer **before** that plugin's `process()` runs.
    /// v29: `device_id` は安定 id (`PluginInstance::id`) — compile 時に Song
    /// から焼き込む。engine は `plugin_refs` (device_id keyed) を直接引く。
    SidechainTap {
        src: BufRef,
        device_id: u64,
        aux_in_port: u8,
    },

    /// r.md #129: 内蔵 device (Comp / Bus Comp) の外部サイドチェイン。`src` の音を `owner`
    /// (song-track index、master は [`MASTER_OWNER`]) の program の `natives[native_slot]` の
    /// 受け皿へ写す (`graph::native::stage_native_sidechain`)。積む位置は plugin の `SidechainTap`
    /// と同じ (track は process の前、master は master `Mix` の後)。
    NativeSidechainTap {
        src: BufRef,
        owner: u32,
        native_slot: u32,
    },

    /// PR4 aux send: accumulate `src` (the source track's post- or
    /// pre-fader buffer) into `dst` (the destination return / bus track's
    /// scratch) scaled by the **live, per-sample-ramped** send gain of
    /// the send with stable id `send_id` on `song.tracks[src_track_idx]`.
    /// Emitted **after** the dst's clearing `Mix` (so it accumulates on top
    /// of any children) and **before** the dst's `ProcessGroupFx`. The gain
    /// is read live (not baked into the schedule) so knob drags / `SendGain`
    /// automation apply without recompiling, and a disabled send contributes
    /// silence. v29: `send_id` は `Send::id` (安定 id)。
    MixSend {
        src: BufRef,
        dst: BufRef,
        src_track_idx: u32,
        send_id: u32,
    },

    /// パラアウト (`docs/plan_paraout.md`): copy the plugin `device_id`'s aux
    /// **output** port `port` (`pd.buffer_aux_out[port]`, written by
    /// daw_plugin_host during the source plugin's pass-1 `process()`) **into**
    /// `dst_track`'s input scratch (`+=`, accumulating after the dst's
    /// clearing `Mix`). The mirror of `SidechainTap`, but the data flows
    /// plugin-out → track-in instead of track-out → plugin-in. Emitted
    /// **before** the dst bus's `ProcessGroupFx` so the dst track's FX process
    /// the routed signal. Zero latency: the source plugin ran in pass 1, so
    /// `buffer_aux_out` is settled by the time this post-dispatch op reads it.
    ParallelOutTap {
        device_id: u64,
        port: u8,
        dst_track: u32,
    },

    /// docs/plan_modulation.md §3: advance the envelope follower for
    /// `ModSource` at `slot` over `src`'s final scratch. Emitted at the end
    /// of the schedule (all scratches are settled) since the follower only
    /// produces a control-rate scalar — it never feeds back into the audio
    /// graph. `slot` は `Schedule::follower_slots` / `follower_keys` /
    /// `mod_kinds` の index。**この slot 番号は schedule の内部表現であって、
    /// 外へ出るときは必ず `follower_keys[slot]` (= `ModSource::id`) に変換する**
    /// (`common/src/mod_plane.rs`、アーキ不変条件 1)。
    EnvelopeFollow { src: BufRef, slot: u32 },
}

/// PDC delay line の stable identity (`docs/plan_arch_refactor.md` §5 D:
/// schedule 再 compile を跨いだ状態移送のキー)。track は Song 上の安定
/// `Track::id`。1 track は高々 1 つの clearing `Mix` (親 bus か master) に
/// しか流れ込まないので、src 側補償は track id 単独で一意。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DelayKey {
    /// Mix 合流点で低 latency 側 src に入る補償 (`emit_mix_src_alignment`)。
    MixSrc { track_id: u32 },
    /// パラアウト `MixAdditive` で dst 自身 (instrument main) を子の最大
    /// latency に揃える補償。
    MixDst { track_id: u32 },
    /// r.md #129: pass 2 で走る bus の consumer が読むサイドチェインに、bus の入力 (子 / send の
    /// 合流) を揃える補償。`ProcessGroupFx` の直前に積む (§8.3.3)。
    BusScAlign { track_id: u32 },
}

/// Compiled, immutable execution plan. Compiled **off the RT thread**
/// (`main.rs` の publish 経路 / export) and delivered to the audio thread
/// inside an `RtBundle` via a wait-free SPSC ring; the RT thread swaps it
/// in with zero allocation and ships the superseded one back for
/// off-thread disposal.
pub struct Schedule {
    /// Ordered list of node ops to execute this buffer.
    pub nodes: Vec<NodeOp>,
    /// PDC delay-line pool (PR3). Indexed by `NodeOp::ApplyDelay::line_idx`.
    pub delay_lines: Vec<DelayLine>,
    /// `delay_lines` と平行な stable key (§5 D 状態移送用)。
    pub delay_keys: Vec<DelayKey>,
    /// Pooled stereo buffers used by ops whose dst is `BufRef::Pooled`.
    /// PR1 leaves this empty.
    pub port_buffers: PortBufferPool,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// in samples, applied **after** vocal/clip render + instrument output
    /// but **before** the audio FX chain. This brings each track's main
    /// signal into musical alignment with its sidechain sources, so a
    /// sidechain plugin sees `main_in` and `aux_in` at the same musical
    /// time.
    ///
    /// Indexed by song track index (parallel to `song.tracks`). Entry `i`
    /// is `max(source latency + buffer_frames)` over the track's **pass-1**
    /// sidechain consumers (leaf の全 device / group-with-instrument の prefix、
    /// plugin の aux 入力と内蔵 device の SC)、or 0 if there are none.
    /// `+ buffer_frames` は「tap の staging が post-dispatch = 消費が次 buffer」という
    /// 1-buffer 遅延の補償 (`docs/plan_arch_refactor.md` §5)。pass 2 (bus の
    /// `ProcessGroupFx`) の consumer はここではなく `ApplyDelay(BusScAlign)` で揃える
    /// (`docs/plan_rack_native_devices.md` §8.3.3)。
    ///
    /// MVP scope: only audio-in+out devices' sidechain is reflected here.
    /// Instrument sidechain alignment requires delaying MIDI events too
    /// (out of scope for PR4.5).
    pub input_delay_per_track: Vec<u32>,
    /// docs/plan_modulation.md §3: per-`ModSource` envelope follower state +
    /// baked coefficients, indexed by slot。`NodeOp::EnvelopeFollow { slot, .. }`
    /// advances `follower_slots[slot].env` each buffer; engine はそれを
    /// `follower_keys[slot]` (= `ModSource::id`) と組にして変調値面へ載せる
    /// (`crate::mod_tick::eval_plane`)。再 compile 時は `adopt_state_from` が
    /// 同じ stable id で走行状態を移送する。
    pub follower_slots: Vec<super::follower::FollowerSlot>,
    /// `follower_slots` と平行な stable `ModSource::id` (§5 D 状態移送用)。
    /// `0` は未採番 sentinel (移送対象外)。
    pub follower_keys: Vec<u32>,
    /// per-`ModSource` の種別を slot 順 (= `follower_slots` / `follower_keys` と
    /// 1:1) に保持。
    /// generator (LFO/Random/MSEG/Steps) は `common::modulators::generator_scalar`
    /// で `song_beat` から直接算出され、その slot の `follower_slots` 値は使われない
    /// (inert)。envelope follower の slot は `follower_slots[slot].env` を使う。
    pub mod_kinds: Vec<common::model::ModSourceKind>,
    /// master **出力** に現れる PDC 遅延量 (samples)。
    /// = master `Mix` の src の `path_latency` 最大値 (= 全 src がこの値に揃えられる)
    ///   **＋ master fx chain 上の device が報告している latency の合計**。
    ///
    /// 言い換えると `master_buffer[P]` に載っているのは曲位置 `P - master_latency_samples`
    /// の音、という写像の遅延量。
    ///
    /// r.md #39 の消費者は 2 つ:
    /// - metronome click は `render_master_buffer` の **後** (= master fx を通さずに) 重ねる
    ///   ので、この値だけ参照位置を戻して他の音と同じ時間軸に揃える
    ///   (REAPER / Ardour もメトロノームを遅延補償の対象にする)。
    /// - WAV 書き出しはこの値だけ書き始めを後ろへずらす (= 先頭の遅延ぶんを捨てる)。
    ///   でないと書き出した wav が丸ごと後ろへずれ、stem を貼り戻すとダブる。
    pub master_latency_samples: u32,
    /// r.md #129: master のフェーダー後 Limiter の先読み遅延を焼いたか
    /// (= `Song::master_limiter_latency_active`: 静的 ON または On レーン / 変調)。
    /// `MasterLimiterState::process` はこの値で遅延を通すかを決めるので、PDC の会計
    /// (`master_latency_samples` の limiter 項) と実際の遅延が食い違わない (§18-G)。
    pub master_limiter_latency: bool,
    /// master の段 (fx chain → SC Listen の置換 → master 音量 → Limiter) を通すか
    /// (`RenderScope::master`、compile 時に焼く)。`false` は全 track の合流をそのまま出力にする (bounce)。
    pub master_stage: bool,
    /// r.md #110 Parallel: track index 順の展開済み device 列 (`docs/plan_parallel.md` §4.1)。
    /// pass 1 (worker) / pass 2 (`ProcessGroupFx`) の両方がこれを走らせる。
    /// scratch (Parallel / chain slot、並列 PDC の delay line) も program が所有する。
    pub track_programs: Vec<ChainProgram>,
    /// master fx chain の program (`track_id = MASTER_TRACK_ID`)。
    pub master_program: ChainProgram,
    /// master program を走らせるときの MIDI バス (master は note を持たないので常に
    /// 空だが、walker の契約上バスが要る)。容量 `MAX_EVENTS` で確保済み。
    pub master_midi_a: Vec<crate::sequencer::TimedNoteEvent>,
    pub master_midi_b: Vec<crate::sequencer::TimedNoteEvent>,
    /// 1 buffer の処理 (track 本体と `nodes`) の依存グラフと、その RT 作業領域
    /// (`docs/plan_parallel_graph.md`)。`nodes` と同じ compile で組む。
    pub graph: super::render_graph::RenderGraph,
    /// solo の透過規則の表 (track index 順)。**render の間は読むだけ** — program (走行状態) とは別に持つので、
    /// 並列実行で ある program を書いている手と、表を読む手 (`MixSend`) が同じ資源を奪い合わない。
    pub solo: SoloTables,
    /// 再 compile 跨ぎの状態移送で、この schedule が「旧」になったときに引く鍵の索引 ([`Self::index_state_keys`])。
    pub state_keys: ScheduleKeys,
}

/// solo の透過規則 (index = song-track index)。配線は compile 時に辺の表へ焼き、どの track が透過するかは
/// **buffer ごとに** その buffer の `solo` から 1 回だけ解く ([`Self::resolve`]、トラック数 + 辺数に比例)。RT で Song の
/// 配線を歩かず、track ごとに推移閉包を舐めもしない (`docs/plan_unbounded_tracks.md` §2.3)。
///
/// 規則は 2 つ:
/// - その track へ **流れ込む** track (子 → group、send 元 → return、send の有効 / 無効を問わない) のどれかが solo なら、
///   solo でない bus も透過する — 「あるトラックを solo すると、そのトラックが送っている reverb / delay の **リターン** も
///   生かす」Ableton 準拠の挙動。
/// - 祖先 group (`parent_group_id` を辿る) のどれかが solo なら透過する — folder solo。
///
/// r.md #131: 実効的に無効なトラックは solo の判定に数えない (`solo_counts`、`Song::solo_counts` と同じ規則)。
/// 流れ込む辺も有効なトラック同士だけ (compile の `Topology` がそう組む)。
#[derive(Debug, Default)]
pub struct SoloTables {
    /// 辺 (流れ込む側 → 受け取る側) を流れ込む側の順に: `flows_to[flows_start[i]..flows_start[i + 1]]`。
    flows_start: Vec<u32>,
    flows_to: Vec<u32>,
    /// song-track index → その track の `solo` を数えるか (= 実効的に有効)。
    solo_counts: Vec<bool>,
    /// 親 group (`u32::MAX` = 無し)。
    parent: Vec<u32>,
    /// 親が子より先に来る並び (祖先の solo を 1 周で子へ降ろす)。
    parent_first: Vec<u32>,
    /// 直近の [`Self::resolve`] の解 (流れ込む track に solo がある / 祖先 group に solo がある)。
    contributor_soloed: Vec<bool>,
    ancestor_soloed: Vec<bool>,
    /// [`Self::resolve`] の作業領域 (容量 2 × トラック数)。
    stack: Vec<u32>,
}

/// track `i` の solo を数えるか。表の外 (compile 前の空の表) は数える (= 無効化を知らない既定)。
fn counts(solo_counts: &[bool], i: usize) -> bool {
    solo_counts.get(i).copied().unwrap_or(true)
}

impl SoloTables {
    /// `flows` = 辺 (流れ込む側, 受け取る側)、`parent[i]` = track `i` の親 group、`enabled[i]` = track `i` が
    /// 実効的に有効か (off-thread)。
    #[must_use]
    pub fn build(flows: impl Iterator<Item = (u32, u32)>, parent: Vec<Option<u32>>, enabled: Vec<bool>) -> Self {
        let n = parent.len();
        let mut edges: Vec<(u32, u32)> = flows.filter(|&(from, to)| (from as usize) < n && (to as usize) < n).collect();
        edges.sort_unstable();
        edges.dedup();
        let mut flows_start = vec![0u32; n + 1];
        for &(from, _) in &edges {
            flows_start[from as usize + 1] += 1;
        }
        for i in 0..n {
            flows_start[i + 1] += flows_start[i];
        }
        let parent: Vec<u32> = parent.into_iter().map(|p| p.filter(|&p| (p as usize) < n).unwrap_or(u32::MAX)).collect();
        let mut kids: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (i, &p) in parent.iter().enumerate() {
            if p != u32::MAX {
                kids[p as usize].push(i as u32);
            }
        }
        // 根から幅優先 (親の循環は compile が弾くので、全 track が並ぶ)。
        let mut parent_first: Vec<u32> = (0..n as u32).filter(|&i| parent[i as usize] == u32::MAX).collect();
        let mut at = 0;
        while at < parent_first.len() {
            let p = parent_first[at] as usize;
            parent_first.extend_from_slice(&kids[p]);
            at += 1;
        }
        Self {
            flows_start,
            flows_to: edges.into_iter().map(|(_, to)| to).collect(),
            solo_counts: (0..n).map(|i| enabled.get(i).copied().unwrap_or(true)).collect(),
            parent,
            parent_first,
            contributor_soloed: vec![false; n],
            ancestor_soloed: vec![false; n],
            stack: Vec::with_capacity(2 * n),
        }
    }

    /// この buffer の `solo` から透過を解く。dispatch の前 (callback スレッド、表を読む手が走る前) に呼ぶ。
    /// RT 安全: 確保済みの表の書き換えのみ。
    pub fn resolve(&mut self, song: &common::model::Song) {
        let Self { flows_start, flows_to, solo_counts, parent, parent_first, contributor_soloed, ancestor_soloed, stack } =
            self;
        let soloed = |i: usize| counts(solo_counts, i) && song.tracks.get(i).is_some_and(|t| t.solo);
        // 流れ込む側: solo の track から辺を前へ辿って届く track に印を付ける。
        contributor_soloed.fill(false);
        stack.clear();
        stack.extend((0..contributor_soloed.len() as u32).filter(|&i| soloed(i as usize)));
        while let Some(from) = stack.pop() {
            let (a, b) = (flows_start[from as usize] as usize, flows_start[from as usize + 1] as usize);
            for &to in &flows_to[a..b] {
                if !std::mem::replace(&mut contributor_soloed[to as usize], true) {
                    stack.push(to);
                }
            }
        }
        // 祖先: 親が先に解けている並びで、親の solo か親の祖先の solo を子へ降ろす。
        ancestor_soloed.fill(false);
        for &i in parent_first.iter() {
            let p = parent[i as usize];
            if p != u32::MAX {
                ancestor_soloed[i as usize] = soloed(p as usize) || ancestor_soloed[p as usize];
            }
        }
    }

    /// この buffer に数える solo が 1 つでもあるか (無効トラックの solo は数えない)。RT 安全: 走査のみ。
    #[must_use]
    pub fn any_solo(&self, song: &common::model::Song) -> bool {
        song.tracks.iter().enumerate().any(|(i, t)| t.solo && counts(&self.solo_counts, i))
    }

    /// track `i` の (流れ込む track に solo がある, 祖先 group に solo がある) — 直近の [`Self::resolve`] の解。範囲外は無し。
    #[must_use]
    pub fn of(&self, i: u32) -> (bool, bool) {
        let i = i as usize;
        (self.contributor_soloed.get(i).copied().unwrap_or(false), self.ancestor_soloed.get(i).copied().unwrap_or(false))
    }
}

impl Schedule {
    /// GR を publish する内蔵 device の数 (track の program + master)。telemetry 面の容量の要求
    /// (`docs/plan_unbounded_tracks.md` §3)。
    #[must_use]
    pub fn native_meter_count(&self) -> usize {
        self.track_programs
            .iter()
            .chain(std::iter::once(&self.master_program))
            .flat_map(|p| p.natives.iter())
            .filter(|ns| ns.meter)
            .count()
    }

    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            delay_lines: Vec::new(),
            delay_keys: Vec::new(),
            port_buffers: PortBufferPool::new(),
            input_delay_per_track: Vec::new(),
            follower_slots: Vec::new(),
            follower_keys: Vec::new(),
            mod_kinds: Vec::new(),
            master_latency_samples: 0,
            master_limiter_latency: false,
            master_stage: true,
            track_programs: Vec::new(),
            master_program: ChainProgram::empty(common::model::MASTER_TRACK_ID),
            master_midi_a: Vec::with_capacity(crate::mixer::MAX_EVENTS),
            master_midi_b: Vec::with_capacity(crate::mixer::MAX_EVENTS),
            graph: super::render_graph::RenderGraph::default(),
            solo: SoloTables::default(),
            state_keys: ScheduleKeys::default(),
        }
    }

    /// §5 D (plan_arch_refactor): topology 再 compile 時の状態移送。旧
    /// schedule から DelayLine (PDC ring の内容) と FollowerSlot (env) を
    /// stable key で引き継ぐ。delay 長が変わった line は移送されず
    /// ゼロ初期化のまま (= リセット)。
    ///
    /// **RT thread 上で呼ばれる** (`LocalState::refresh_bundle` の install
    /// 時)。live の走行状態 (ring の音声履歴 / env) は RT だけが持つので、
    /// off-thread では移送できない — ここで行う操作は `Vec` の `mem::swap`
    /// (ポインタ交換) と f32 コピーだけで、alloc / free / lock は無い。
    /// 突き合わせは旧 schedule の鍵の索引 ([`KeyIndex`]、compile 時に作ってある) — 本数に上限が無いので、鍵が
    /// 入れ替わった / 消えた要素があっても二乗にしない。
    pub fn adopt_state_from(&mut self, old: &mut Schedule) {
        let mut hint = 0;
        for (i, key) in self.delay_keys.iter().enumerate() {
            if let Some(j) = old.state_keys.delay.find_near(&old.delay_keys, *key, hint) {
                // 補償 delay 長が変わっていても手元の過去は引き継ぐ (`DelayLine::adopt`)。
                self.delay_lines[i].adopt(&mut old.delay_lines[j]);
                hint = j + 1;
            }
        }
        hint = 0;
        for (i, key) in self.follower_keys.iter().enumerate() {
            if *key == 0 {
                continue; // 未採番 sentinel は identity にならない
            }
            if let Some(j) = old.state_keys.followers.find_near(&old.follower_keys, *key, hint) {
                self.follower_slots[i].adopt_state_from(&old.follower_slots[j]);
                hint = j + 1;
            }
        }
        // r.md #110: Parallel の並列 PDC ring と chain の tap snapshot は所有 track id →
        // chain id で移送する (`ChainProgram::adopt_state_from`)。
        hint = 0;
        for p in &mut self.track_programs {
            let found = old.state_keys.programs.find_near_by(p.track_id, hint, |j| old.track_programs.get(j).map(|o| o.track_id));
            if let Some(j) = found {
                p.adopt_state_from(&mut old.track_programs[j]);
                hint = j + 1;
            }
        }
        self.master_program.adopt_state_from(&mut old.master_program);
    }

    /// [`Self::adopt_state_from`] の鍵の索引を作る (off-thread、schedule を組み終えた後。program の分は
    /// `build_program` が作っている)。
    pub fn index_state_keys(&mut self) {
        self.state_keys = ScheduleKeys {
            delay: KeyIndex::build(self.delay_keys.iter().copied()),
            followers: KeyIndex::build(self.follower_keys.iter().copied()),
            programs: KeyIndex::build(self.track_programs.iter().map(|p| p.track_id)),
        };
    }
}

/// [`Schedule::adopt_state_from`] の突き合わせの鍵の索引。
#[derive(Debug, Default)]
pub struct ScheduleKeys {
    delay: KeyIndex<DelayKey>,
    followers: KeyIndex<u32>,
    programs: KeyIndex<u32>,
}

/// 再 compile を跨ぐ状態移送の、鍵 → 要素の位置の索引 (off-thread で作る)。
#[derive(Debug)]
pub struct KeyIndex<K> {
    /// `(鍵, 位置)` を鍵 → 位置の順に。
    sorted: Vec<(K, u32)>,
}

impl<K> Default for KeyIndex<K> {
    fn default() -> Self {
        Self { sorted: Vec::new() }
    }
}

impl<K: Ord + Copy> KeyIndex<K> {
    #[must_use]
    pub fn build(keys: impl Iterator<Item = K>) -> Self {
        let mut sorted: Vec<(K, u32)> = keys.enumerate().map(|(i, k)| (k, u32::try_from(i).unwrap_or(u32::MAX))).collect();
        sorted.sort_unstable();
        Self { sorted }
    }

    /// 鍵 `key` の要素 (`keys[j] == key`) のうち、位置が `hint` 以上で最初のもの、無ければ最初のもの。新旧の並びが
    /// ほぼ同じ突き合わせで前回の一致位置の次を `hint` に渡すと、同じ鍵が複数あっても並び順に対になる。
    /// `keys` は索引を作った列 (答えは必ず `keys` で照合するので、別の列の索引から外れた位置は返らない)。
    /// 確保なし (RT 安全)。
    pub fn find_near(&self, keys: &[K], key: K, hint: usize) -> Option<usize> {
        self.find_near_by(key, hint, |j| keys.get(j).copied())
    }

    /// [`Self::find_near`] の、位置 → 鍵を関数で引く版。
    pub fn find_near_by(&self, key: K, hint: usize, key_at: impl Fn(usize) -> Option<K>) -> Option<usize> {
        let lo = self.sorted.partition_point(|(k, _)| *k < key);
        let run = &self.sorted[lo..];
        let run = &run[..run.partition_point(|(k, _)| *k == key)];
        let at = run.partition_point(|&(_, pos)| (pos as usize) < hint);
        let j = run.get(at).or_else(|| run.first())?.1 as usize;
        (key_at(j) == Some(key)).then_some(j)
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::KeyIndex;

    /// 鍵の索引は「`hint` から後ろで最初 → 無ければ先頭から最初」の線形探索と同じ位置を返す
    /// (同じ鍵が複数ある / 鍵が無い / `hint` が末尾を越える)。
    #[test]
    fn 鍵の索引は_hint_から探す線形探索と同じ位置を返す() {
        let keys = [5u32, 3, 5, 9, 3, 5];
        let index = KeyIndex::build(keys.iter().copied());
        let linear = |key: u32, hint: usize| {
            let hint = hint.min(keys.len());
            keys[hint..]
                .iter()
                .position(|k| *k == key)
                .map(|p| hint + p)
                .or_else(|| keys[..hint].iter().position(|k| *k == key))
        };
        for key in [3, 5, 9, 7] {
            for hint in 0..=keys.len() + 1 {
                assert_eq!(index.find_near(&keys, key, hint), linear(key, hint), "key={key} hint={hint}");
            }
        }
        assert_eq!(index.find_near(&[5, 3], 9, 0), None, "別の列の索引から外れた位置は返さない");
    }
}
