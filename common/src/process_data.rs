//! Shared-memory layout for a single plugin instance's `process()` call.
//!
//! `daw_audio` writes the inputs (frames, events, buffer_in), signals the
//! plugin host via `event_request`, the host runs `plugin.process()` filling
//! the outputs (events_out, buffer_out), then signals `event_response`.
//!
//! Sizes are fixed at compile time so the whole struct is plain old data
//! living in shared memory: no allocations, no descriptors, no headers.
//!
//! Multiple plugin instances each get their own `ProcessData` slot (one
//! shmem region + one event pair per plugin), so worker threads on the
//! audio side can hand work to different plugins concurrently.

pub const MAX_FRAMES: usize = 1024;
pub const MAX_CHANNELS: usize = 2;
pub const MAX_EVENTS: usize = 256;
/// PR4 sidechain: how many `is_main=false` aux input ports per plugin
/// the host reserves shmem for. 1 covers the typical "single sidechain
/// trigger" use case (compressor / gate / ducker); we allocate one
/// extra slot (= 2) to leave room for plugins that expose 2 separate
/// keying inputs without a host upgrade.
pub const MAX_AUX_IN: usize = 2;
/// パラアウト (`docs/plan_paraout.md`): how many `is_main=false` aux **output**
/// ports per plugin the host reserves shmem for. 16 covers a full multi-out
/// drum instrument (MeldaProduction MDrummer = main + 15 stereo part buses;
/// VCV Rack; Battery / Geist); plugins that declare more have the extras
/// ignored (the host warns at load). Symmetric to `MAX_AUX_IN`. Cost:
/// `16 * 2ch * 1024f * 4B = 128 KB`/plugin (妥当)。 Bumping this is an ABI
/// break (shmem layout) → `cargo build --workspace`.
pub const MAX_AUX_OUT: usize = 16;

#[repr(C)]
pub struct ProcessData {
    /// How many frames to process this call (`<= MAX_FRAMES`).
    pub frames: u32,
    /// Sample counter monotonically advanced by the audio engine. Fed into
    /// the CLAP transport so plugins see a consistent timeline.
    pub steady_time: u64,
    /// Sample rate the engine was activated at (Hz). Stored so the plugin
    /// host can pass it through the CLAP process struct without a separate
    /// IPC hop.
    pub sample_rate: u32,
    /// Whether the transport is rolling. Lets plugins distinguish "render
    /// silence" from "host is paused".
    pub playing: u8,
    pub _pad0: [u8; 3],

    pub n_events_in: u32,
    pub events_in: [Event; MAX_EVENTS],
    pub n_events_out: u32,
    pub events_out: [Event; MAX_EVENTS],

    /// Planar f32 input audio (channel × frame).
    pub buffer_in: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    /// Planar f32 output audio.
    pub buffer_out: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    /// PR4 sidechain: planar f32 aux input audio
    /// (port × channel × frame). Filled by the audio engine's
    /// `NodeOp::SidechainTap` handler before plugin.process(). The
    /// per-port `aux_in_active` flag tells the plugin host whether to
    /// pass that port to the plugin (CLAP `clap_audio_buffer` /
    /// VST3 `AudioBusBuffers`); inactive ports are skipped or fed
    /// silence so plugins that don't request a sidechain stay quiet.
    pub buffer_aux_in: [[[f32; MAX_FRAMES]; MAX_CHANNELS]; MAX_AUX_IN],
    /// 1 = aux input port is wired up this buffer, 0 = no source set
    /// (plugin host should pass silence / null buffer to the plugin).
    pub aux_in_active: [u8; MAX_AUX_IN],
    /// Pad to keep the next field aligned (struct must remain a plain
    /// data layout for shmem). Sum: `MAX_AUX_IN * sizeof(u8) + pad`
    /// reaches the next u32 boundary.
    pub _pad_aux: [u8; 2],
    /// パラアウト (`docs/plan_paraout.md`): planar f32 aux **output** audio
    /// (port × channel × frame). Written by `daw_plugin_host` after
    /// `plugin.process()` — each `is_main=false` output port the plugin
    /// declared is copied here. The audio engine's `NodeOp::ParallelOutTap`
    /// handler then mixes a routed port into its destination track's input
    /// (post-dispatch, same buffer = zero latency). Symmetric to
    /// `buffer_aux_in`.
    pub buffer_aux_out: [[[f32; MAX_FRAMES]; MAX_CHANNELS]; MAX_AUX_OUT],
    /// 1 = the plugin declared this aux output port (host filled
    /// `buffer_aux_out[port]` this buffer); 0 = no such port (engine must
    /// not read it). Set by `daw_plugin_host` from the CLAP `audio_ports`
    /// scan. `MAX_AUX_OUT * sizeof(u8) = 4` already lands on a u32 boundary,
    /// so no trailing pad is needed before `bpm`.
    pub aux_out_active: [u8; MAX_AUX_OUT],
    /// Phase 5 Step 5.3 (`docs/plan_automation.md` §10): CLAP plugin の
    /// `clap_event_transport` 構築に使う song-level transport state。
    /// daw_audio が buffer 頭で song 情報から populate。 plugin host は
    /// `bpm` / `tsig_num` / `tsig_denom` と既存の `steady_time` (sample
    /// 単位 playhead) + `sample_rate` から `clap_event_transport` を組み立て、
    /// `clap_process.transport` に渡す。 plugin の tempo-sync 機能
    /// (sync to beat delay / arp 等) が動作する基盤。
    pub bpm: f32,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    /// Phase 5 Step 5.3: loop 区間 (beats)。 audio engine の loop state を
    /// plugin に伝えて CLAP `CLAP_TRANSPORT_IS_LOOP_ACTIVE` の判定に使う。
    /// 旧 v 互換性のため `f64` で stable layout を保つ。 `loop_end_beats >
    /// loop_start_beats` なら loop active と解釈。
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    /// 累積拍位置 (= buffer 頭の song 位置 in beats)。 audio engine が
    /// `playhead_beats` (tempo automation を積分した真の拍位置) をそのまま
    /// 格納する。 plugin host はこれを CLAP `song_pos_beats` / VST3
    /// `projectTimeMusic` に**直接**使う (= `steady_time × bpm` の一定テンポ
    /// 逆算を廃止)。 これでテンポオートメーション中も plugin の tempo-sync が
    /// 正しい拍に追従する。 `song_pos_seconds` は sample 由来 (= テンポ非依存で
    /// 正確) なので別途格納せず host 側で `steady_time / sample_rate` を使う。
    pub song_pos_beats: f64,
    /// Phase 5 Step 5.3: `clap_event_transport.flags` の `IS_LOOPING` ビット
    /// に使う「user が loop button を押している」 状態。 `loop_end_beats >
    /// loop_start_beats` だけでは「loop region は設定したが loop button は
    /// off」 ケースと区別できないので独立フラグで持つ。
    pub looping: u8,
    pub _pad_transport: [u8; 5],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub _pad: [u8; 3],
    /// Frame offset within the buffer (`< frames`).
    pub time: u32,
    /// Note number (NoteOn / NoteOff). Param events ignore this.
    pub key: u8,
    pub channel: u8,
    pub _pad1: [u8; 2],
    /// Velocity (NoteOn) — ignored otherwise.
    pub velocity: f64,
    /// CLAP param id (Param events) — 0 for note events.
    pub param_id: u32,
    /// PR-V2.4: stable per-note identifier propagated from
    /// `daw_audio::sequencer` to plugins. Builtin plugins
    /// (`PluginFormat::Builtin`, e.g. VOICEVOX) use this to look up
    /// per-note metadata (歌詞 / phoneme) and synthesised audio
    /// caches. CLAP backends forward it to `clap_event_note.note_id`;
    /// VST3 backends ignore it. `0` is a valid id (= first note in
    /// the track), so consumers should not treat 0 as "unset".
    pub note_id: u32,
    /// Param value (Param events) — ignored otherwise.
    pub value: f64,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    NoteOn = 1,
    NoteOff = 2,
    /// Absolute parameter value (automation / direct set). `value` is in the
    /// daw's parameter domain (plugin host converts per-format to CLAP plain /
    /// VST3 normalized).
    ParamValue = 3,
    /// **lane 非依存モジュレーション** (`docs/plan_modulation_routing_redesign.md`
    /// §3.2): a *normalized* (`-1..=1`) modulation offset for `param_id`. The
    /// plugin host applies it per-format — CLAP modulatable params get a
    /// non-destructive `clap_event_param_mod` (`amount = value·(max−min)`),
    /// other params have it folded into the absolute value the host sends.
    /// `value` carries the offset.
    ParamMod = 4,
}

impl ProcessData {
    pub const fn empty() -> Self {
        Self {
            frames: 0,
            steady_time: 0,
            sample_rate: 48_000,
            playing: 0,
            _pad0: [0; 3],
            n_events_in: 0,
            events_in: [EMPTY_EVENT; MAX_EVENTS],
            n_events_out: 0,
            events_out: [EMPTY_EVENT; MAX_EVENTS],
            buffer_in: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
            buffer_out: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
            buffer_aux_in: [[[0.0; MAX_FRAMES]; MAX_CHANNELS]; MAX_AUX_IN],
            aux_in_active: [0; MAX_AUX_IN],
            _pad_aux: [0; 2],
            buffer_aux_out: [[[0.0; MAX_FRAMES]; MAX_CHANNELS]; MAX_AUX_OUT],
            aux_out_active: [0; MAX_AUX_OUT],
            bpm: 120.0,
            tsig_num: 4,
            tsig_denom: 4,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
            song_pos_beats: 0.0,
            looping: 0,
            _pad_transport: [0; 5],
        }
    }

    /// Reset event counts and silence input/output buffers headers. Called
    /// before each dispatch so stale events from a previous buffer don't
    /// leak through.
    pub fn prepare(&mut self) {
        self.n_events_in = 0;
        self.n_events_out = 0;
    }

    /// Push a NoteOn into `events_in`. Silently truncates if the buffer is
    /// full — at MAX_EVENTS=256 per buffer this should never happen for
    /// normal MIDI traffic, and panicking inside RT is worse than dropping.
    /// PR-V2.4: `note_id` carries the per-note identifier so builtin
    /// plugins (VOICEVOX) can look up `NoteMetadata` (歌詞) and
    /// synthesised audio caches keyed by `note_id`.
    pub fn push_note_on(
        &mut self,
        time: u32,
        key: u8,
        velocity: f64,
        channel: u8,
        note_id: u32,
    ) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::NoteOn,
            _pad: [0; 3],
            time,
            key,
            channel,
            _pad1: [0; 2],
            velocity,
            param_id: 0,
            note_id,
            value: 0.0,
        };
        self.n_events_in += 1;
    }

    pub fn push_note_off(&mut self, time: u32, key: u8, channel: u8, note_id: u32) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::NoteOff,
            _pad: [0; 3],
            time,
            key,
            channel,
            _pad1: [0; 2],
            velocity: 0.0,
            param_id: 0,
            note_id,
            value: 0.0,
        };
        self.n_events_in += 1;
    }

    pub fn push_param(&mut self, time: u32, param_id: u32, value: f64) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::ParamValue,
            _pad: [0; 3],
            time,
            key: 0,
            channel: 0,
            _pad1: [0; 2],
            velocity: 0.0,
            param_id,
            note_id: 0,
            value,
        };
        self.n_events_in += 1;
    }

    /// Push a **normalized modulation offset** for `param_id`
    /// (`docs/plan_modulation_routing_redesign.md` §3.2). `offset_norm` is in
    /// the `-1..=1` normalized domain; the plugin host converts it per-format
    /// (CLAP `param_mod` for modulatable params, else folded into the absolute
    /// value). Same RT-safe truncation contract as `push_param`.
    pub fn push_param_mod(&mut self, time: u32, param_id: u32, offset_norm: f64) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::ParamMod,
            _pad: [0; 3],
            time,
            key: 0,
            channel: 0,
            _pad1: [0; 2],
            velocity: 0.0,
            param_id,
            note_id: 0,
            value: offset_norm,
        };
        self.n_events_in += 1;
    }
}

const EMPTY_EVENT: Event = Event {
    kind: EventKind::NoteOn,
    _pad: [0; 3],
    time: 0,
    key: 0,
    channel: 0,
    _pad1: [0; 2],
    velocity: 0.0,
    param_id: 0,
    note_id: 0,
    value: 0.0,
};

#[cfg(windows)]
mod shmem_handle {
    use anyhow::{Context, Result};
    use shared_memory::{Shmem, ShmemConf};

    use super::ProcessData;

    /// Owning handle to a `ProcessData` shared memory region. The audio
    /// engine creates it; the plugin host opens it by the same id.
    pub struct ProcessDataHandle {
        shmem: Shmem,
    }

    impl ProcessDataHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .size(std::mem::size_of::<ProcessData>())
                .os_id(os_id)
                .create()
                .with_context(|| format!("failed to create shmem {os_id}"))?;
            // Zero the region so the reading side never sees uninit memory.
            unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, std::mem::size_of::<ProcessData>()) };
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .os_id(os_id)
                .open()
                .with_context(|| format!("failed to open shmem {os_id}"))?;
            anyhow::ensure!(
                shmem.len() >= std::mem::size_of::<ProcessData>(),
                "shmem too small for ProcessData: {} < {}",
                shmem.len(),
                std::mem::size_of::<ProcessData>()
            );
            Ok(Self { shmem })
        }

        pub fn ptr(&self) -> *mut ProcessData {
            self.shmem.as_ptr() as *mut ProcessData
        }
    }

    // The single ProcessData slot is exclusively written by the audio
    // engine (inputs) and the plugin host worker (outputs); the named
    // event handshake serialises every access.
    unsafe impl Send for ProcessDataHandle {}
    unsafe impl Sync for ProcessDataHandle {}
}

#[cfg(windows)]
pub use shmem_handle::ProcessDataHandle;
