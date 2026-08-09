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
}

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
    /// `start_device` = the first index in `Track.devices` to run. `0` for a
    /// pure group / return (its whole chain is bus FX). For a **パラアウト
    /// group-with-instrument** (`docs/plan_paraout.md`) the instrument prefix
    /// `[0..start_device]` already ran in pass 1 (`process_track_owned`,
    /// producing the main signal + aux outputs), so this op runs only the
    /// suffix FX `[start_device..]` on the summed bus (instrument main +
    /// children) — that's how "the instrument track's own FX process the whole
    /// kit" is realised.
    ProcessGroupFx { track_idx: u32, start_device: u32 },

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
    /// graph. `slot` indexes both `Schedule::follower_slots` and the
    /// `AudioBridge::mod_scalars` plane (= the `ModSource`'s position in
    /// `Song::mod_sources`).
    EnvelopeFollow { src: BufRef, slot: u32 },
}

/// PDC delay line の stable identity (`docs/plan_arch_refactor.md` §5 D:
/// schedule 再 compile を跨いだ状態移送のキー)。track は Song 上の安定
/// `Track::id`。1 track は高々 1 つの clearing `Mix` (親 bus か master) に
/// しか流れ込まないので、src 側補償は track id 単独で一意。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayKey {
    /// Mix 合流点で低 latency 側 src に入る補償 (`emit_mix_src_alignment`)。
    MixSrc { track_id: u32 },
    /// パラアウト `MixAdditive` で dst 自身 (instrument main) を子の最大
    /// latency に揃える補償。
    MixDst { track_id: u32 },
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
    /// is `max(path_latency(src) [+ buffer_frames for a leaf dst] for src
    /// in devices[*].aux_inputs[*].tap)`, or 0 if the track has no
    /// sidechain wiring. leaf dst の `+ buffer_frames` は「tap の staging が
    /// post-dispatch = 消費が次 buffer」という 1-buffer 遅延の補償
    /// (`docs/plan_arch_refactor.md` §5)。
    ///
    /// MVP scope: only audio-in+out devices' sidechain is reflected here.
    /// Instrument sidechain alignment requires delaying MIDI events too
    /// (out of scope for PR4.5).
    pub input_delay_per_track: Vec<u32>,
    /// docs/plan_modulation.md §3: per-`ModSource` envelope follower state +
    /// baked coefficients, indexed by slot (= `ModSource` position in
    /// `Song::mod_sources`). `NodeOp::EnvelopeFollow { slot, .. }` advances
    /// `follower_slots[slot].env` each buffer; the engine publishes that env
    /// to `AudioBridge::mod_scalars[slot]`. 再 compile 時は
    /// `adopt_state_from` が stable id (`follower_keys`) で走行状態を移送する。
    pub follower_slots: Vec<super::follower::FollowerSlot>,
    /// `follower_slots` と平行な stable `ModSource::id` (§5 D 状態移送用)。
    /// `0` は未採番 sentinel (移送対象外)。
    pub follower_keys: Vec<u32>,
    /// per-`ModSource` の種別を
    /// slot 順 (= `follower_slots` / `AudioBridge::mod_scalars` と 1:1) に保持。
    /// generator (LFO/Random/MSEG/Steps) は `common::modulators::generator_scalar`
    /// で `song_beat` から直接算出され、その slot の `follower_slots` 値は使われない
    /// (inert)。envelope follower の slot は `follower_slots[slot].env` を使う。
    pub mod_kinds: Vec<common::model::ModSourceKind>,
    /// master bus に到達する音の PDC 遅延量 (samples) = master `Mix` の src の
    /// `path_latency` 最大値 (= 全 src がこの値に揃えられる)。
    ///
    /// r.md #39: metronome click は `render_master_buffer` の **後** に生の playhead で
    /// 重ねるため、latency を報告するプラグインが 1 つでもあると click だけが早く鳴る。
    /// engine はこの値だけ click の参照位置を戻して、他の音と同じ時間軸に揃える
    /// (REAPER / Ardour もメトロノームを遅延補償の対象にする)。
    pub master_latency_samples: u32,
}

impl Schedule {
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
    /// 探索は小さい Vec の線形走査 (delay lines ≤ track 数、followers ≤
    /// MAX_MOD_SOURCES)。
    pub fn adopt_state_from(&mut self, old: &mut Schedule) {
        for (i, key) in self.delay_keys.iter().enumerate() {
            if let Some(j) = old.delay_keys.iter().position(|k| k == key) {
                // capacity 不一致 (= 補償 delay 長が変わった) なら false =
                // リセットのまま。
                let _ = self.delay_lines[i].try_adopt(&mut old.delay_lines[j]);
            }
        }
        for (i, key) in self.follower_keys.iter().enumerate() {
            if *key == 0 {
                continue; // 未採番 sentinel は identity にならない
            }
            if let Some(j) = old.follower_keys.iter().position(|k| k == key) {
                self.follower_slots[i].adopt_state_from(&old.follower_slots[j]);
            }
        }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::empty()
    }
}
