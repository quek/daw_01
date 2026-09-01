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
/// r.md #89 (`docs/plan_rmd_88_89_cross_modulation.md` §2.2): capacity of the
/// **dedicated** parameter-modulation array.
///
/// 変調は 1 buffer 1 発ではなく制御グリッド (64 サンプル刻み = 1024 frame buffer で
/// 最大 16 刻み) × 変調中の param 数ぶん出る。これを `events_in` (`MAX_EVENTS` = 256)
/// に相乗りさせていると、溢れたぶんが黙って捨てられる先に**ノートが並んでいる** —
/// 変調が NoteOff を押し出してハングノートになる。専用配列に分けて、その事故を
/// 構造的に起こせなくする。
///
/// 容量は 16 刻み × 64 param。1 件 16 バイトなので 16 KB / plugin instance。
pub const MAX_PARAM_MODS: usize = 1024;

/// RT の event 変換バッファの推奨容量 = ノート ([`MAX_EVENTS`]) と param modulation
/// ([`MAX_PARAM_MODS`]) が同時に満杯でも確保が起きない大きさ。
/// **plugin host 側の事前確保はこの 1 本を引くこと** — 片方の上限だけ上げると、
/// 変換先の `Vec` が audio worker thread で realloc する (r.md #89)。
pub const MAX_RT_EVENT_BUFFER: usize = MAX_EVENTS + MAX_PARAM_MODS;
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

    /// r.md #89: **lane 非依存モジュレーション専用**の配列 (`events_in` とは別枠)。
    /// 有効なのは [`ProcessData::param_mods_iter`] が返す `n_param_mods` 件で、
    /// `param_mods_head` から始まるリングとして並ぶ。
    pub param_mods: [ParamMod; MAX_PARAM_MODS],
    /// 有効件数 (`<= MAX_PARAM_MODS`)。
    pub n_param_mods: u32,
    /// リングの先頭 (= 最も古い有効要素) の index。溢れていなければ `0`。
    pub param_mods_head: u32,
    /// 溢れて捨てた件数 (この buffer 内)。`prepare()` で 0 に戻る。
    /// 0 以外なら制御グリッドの前半が落ちている (最新は残る)。
    pub param_mods_dropped: u32,
    pub _pad_param_mods: [u8; 4],

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
    pub _pad_transport: [u8; 7],
    /// r.md #87: **この device が載っている「行」の時間軸**。
    /// `song_pos_beats` (曲全体の位置) と意味が違うので型で分けてある。
    pub row: RowTransport,
}

/// r.md #87 (クリップランチャー): 1 行ぶんの時間軸
/// (`docs/plan_rmd_87_clip_launcher.md` §2.1)。
///
/// ランチャーは「行ごとに時間軸の供給元を切り替える機構」なので、行の主導権を
/// ランチャーが握っている間、その行の device が見るべき musical time は
/// **曲の拍ではなくセルの実効拍**。engine (`daw_audio::launcher::RowPhase`) が
/// 毎 buffer 解いた結果をここへ載せて plugin host へ渡す。
///
/// **位相の式 (`launch_beat + ((playhead - launch_beat) mod loop_len)`) の SSoT は
/// engine 1 本**で、プロセス境界の向こうでは書き直さない — ここに載るのは
/// 解決済みの値だけ。値は **buffer 頭のもの**で、buffer 内のループ端跨ぎは
/// 反映しない (アレンジのループ wrap も buffer 境界でしか起きない = 同じ粒度)。
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct RowTransport {
    /// buffer 頭におけるこの行の実効拍。ランチャーが主導権を持たない行
    /// (= アレンジ) では `ProcessData::song_pos_beats` と一致する。
    pub pos_beats: f64,
    /// **この行自身のループ区間** (拍)。セルを鳴らしている行はセルの窓
    /// `[cell_start, cell_start + loop_len)`、ワンショットのセルと
    /// アレンジ主導の行は `0.0 / 0.0` (= 自前のループ無し)。
    ///
    /// `pos_beats` がセルの窓の中を回っているのに曲のループ区間を名乗り続けると、
    /// plugin から見て「拍がループの外を回っている」不整合になるので、位置と
    /// 対で運ぶ。曲のループ (`ProcessData::loop_*`) を置き換えるものではない
    /// — アレンジ主導の行ではそちらがそのまま使われる。
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    /// 行のイベント源。`0` = アレンジのタイムライン、それ以外 = いま鳴っている
    /// セルの `Clip::id` (`Clip::id` は 1 から採番されるので 0 と衝突しない)。
    pub cell_clip_id: u32,
    /// `1` = この buffer でこの行は無音 (Stop Clips / 空セルのシーンを撃った /
    /// ワンショットが終端を越えた)。**行のタイムラインから音を作る device**
    /// (builtin VOICEVOX の連続再生) は出力を止める。エフェクトは止めない
    /// — 直前まで鳴っていた音のテールを切ってしまう。
    pub silent: u8,
    pub _pad: [u8; 3],
}

/// [`RowTransport::cell_clip_id`] が「アレンジのタイムライン」を表す値。
/// engine (書き手) と plugin host (読み手) で同じ意味を持つ必要があるので
/// 定義はここ 1 本。
pub const ARRANGER_CELL_ID: u32 = 0;

impl RowTransport {
    /// 供給元がアレンジのタイムラインか (= セルを鳴らしていない)。
    #[must_use]
    #[inline]
    pub fn is_arrangement(self) -> bool {
        self.cell_clip_id == ARRANGER_CELL_ID
    }

    /// この buffer でこの行は無音か。
    #[must_use]
    #[inline]
    pub fn is_silent(self) -> bool {
        self.silent != 0
    }

    /// この行が自前のループ区間を持つか (= ループするセルを鳴らしている)。
    #[must_use]
    #[inline]
    pub fn has_loop(self) -> bool {
        self.loop_end_beats > self.loop_start_beats
    }
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
    /// VST3 backends ignore it. `0` is a valid id
    /// (`plugin_metadata::sing_note_id(0, 0)`), so consumers must not
    /// treat 0 as "unset".
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
}

/// **lane 非依存モジュレーション** 1 件
/// (`docs/plan_modulation_routing_redesign.md` §3.2 /
/// `docs/plan_rmd_88_89_cross_modulation.md` §2.2)。
///
/// `value` は正規化 (`-1..=1`) オフセット。plugin host が per-format に適用する
/// — CLAP の modulatable param には非破壊の `clap_event_param_mod`
/// (`amount = value·(max−min)`)、それ以外は絶対値へ畳み込む。
///
/// `events_in` の [`Event`] と分けてあるのは容量の共有を断つため
/// ([`MAX_PARAM_MODS`] の doc)。
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ParamMod {
    /// Frame offset within the buffer (`< frames`).
    pub time: u32,
    pub param_id: u32,
    /// Normalized (`-1..=1`) offset.
    pub value: f64,
}

const EMPTY_PARAM_MOD: ParamMod = ParamMod {
    time: 0,
    param_id: 0,
    value: 0.0,
};

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
            param_mods: [EMPTY_PARAM_MOD; MAX_PARAM_MODS],
            n_param_mods: 0,
            param_mods_head: 0,
            param_mods_dropped: 0,
            _pad_param_mods: [0; 4],
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
            _pad_transport: [0; 7],
            row: RowTransport {
                pos_beats: 0.0,
                loop_start_beats: 0.0,
                loop_end_beats: 0.0,
                cell_clip_id: 0,
                silent: 0,
                _pad: [0; 3],
            },
        }
    }

    /// Reset event counts and silence input/output buffers headers. Called
    /// before each dispatch so stale events from a previous buffer don't
    /// leak through.
    pub fn prepare(&mut self) {
        self.n_events_in = 0;
        self.n_events_out = 0;
        self.n_param_mods = 0;
        self.param_mods_head = 0;
        self.param_mods_dropped = 0;
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

    /// Push a **normalized modulation offset** for `param_id` into the
    /// dedicated [`ProcessData::param_mods`] array
    /// (`docs/plan_modulation_routing_redesign.md` §3.2 /
    /// `docs/plan_rmd_88_89_cross_modulation.md` §2.2). `offset_norm` is in the
    /// `-1..=1` normalized domain; the plugin host converts it per-format
    /// (CLAP `param_mod` for modulatable params, else folded into the absolute
    /// value).
    ///
    /// 溢れたときは **最も古い 1 件を捨てて最新を入れる** (先頭を進めるだけの
    /// O(1)。捨てるのは古い側 = 制御グリッドの前半で、buffer 末に効く最新の
    /// offset は必ず残る)。黙って**新しい方**を捨てると解除されない mod offset が
    /// 居座るので、向きはこちらでなければならない。
    ///
    /// RT 安全: 固定長配列への書き込みのみ (確保・ロック無し)。
    pub fn push_param_mod(&mut self, time: u32, param_id: u32, offset_norm: f64) {
        let n = self.n_param_mods as usize;
        let head = self.param_mods_head as usize % MAX_PARAM_MODS;
        let slot = if n < MAX_PARAM_MODS {
            self.n_param_mods += 1;
            (head + n) % MAX_PARAM_MODS
        } else {
            // **ここで tracing を呼ばない。** `push_param_mod` は audio callback から
            // 走るので、subscriber がファイルへ書いていればその buffer で I/O
            // ブロックする (CLAUDE.md「RT では I/O 禁止」)。溢れは
            // `param_mods_dropped` に数えるだけにして、報告は off-RT の読み手に任せる。
            self.param_mods_dropped = self.param_mods_dropped.saturating_add(1);
            self.param_mods_head = ((head + 1) % MAX_PARAM_MODS) as u32;
            head
        };
        self.param_mods[slot] = ParamMod {
            time,
            param_id,
            value: offset_norm,
        };
    }

    /// 積んだ順 (= `time` 昇順) に param modulation を読む。
    ///
    /// shmem 越しに届いた `n_param_mods` / `param_mods_head` は信頼境界の外なので
    /// ここで clamp する (`process_server` が `frames` を clamp するのと同じ規約)。
    pub fn param_mods_iter(&self) -> impl Iterator<Item = &ParamMod> + '_ {
        let head = self.param_mods_head as usize % MAX_PARAM_MODS;
        let n = (self.n_param_mods as usize).min(MAX_PARAM_MODS);
        (0..n).map(move |i| &self.param_mods[(head + i) % MAX_PARAM_MODS])
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
    use anyhow::Result;

    use super::ProcessData;
    use crate::shmem::NamedShmem;

    /// Owning handle to a `ProcessData` shared memory region. The audio
    /// engine creates it; the plugin host opens it by the same id.
    pub struct ProcessDataHandle {
        shmem: NamedShmem,
    }

    impl ProcessDataHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::create(os_id, std::mem::size_of::<ProcessData>())?;
            // Zero the region so the reading side never sees uninit memory.
            unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, std::mem::size_of::<ProcessData>()) };
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::open(os_id, std::mem::size_of::<ProcessData>())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #89: **溢れたら古い側が落ち、最新は必ず残る。**
    /// 逆向き (新しい方を捨てる) だと、変調が 0 に戻る最後の offset が届かず
    /// 解除されない mod がパラメータに居座る。
    #[test]
    fn param_mods_が溢れたら古い側から捨てる() {
        let mut pd = ProcessData::empty();
        pd.prepare();
        // 容量 +3 件積む。
        for i in 0..(MAX_PARAM_MODS + 3) {
            pd.push_param_mod(i as u32, 1, i as f64);
        }
        assert_eq!(pd.n_param_mods as usize, MAX_PARAM_MODS);
        assert_eq!(pd.param_mods_dropped, 3);
        let got: Vec<f64> = pd.param_mods_iter().map(|m| m.value).collect();
        assert_eq!(got.len(), MAX_PARAM_MODS);
        // 先頭は 3 件ぶん進み、末尾は最後に積んだもの。
        assert_eq!(got[0], 3.0);
        assert_eq!(got[MAX_PARAM_MODS - 1], (MAX_PARAM_MODS + 2) as f64);
        // prepare() でリングごと巻き戻る (次 buffer に持ち越さない)。
        pd.prepare();
        assert_eq!(pd.param_mods_iter().count(), 0);
        assert_eq!(pd.param_mods_head, 0);
    }

    /// 変調をいくら積んでも `events_in` (ノート枠) を食わない
    /// — 分離したことの唯一の目的。
    #[test]
    fn param_mods_は_events_in_を消費しない() {
        let mut pd = ProcessData::empty();
        pd.prepare();
        for i in 0..MAX_PARAM_MODS {
            pd.push_param_mod(0, i as u32, 0.5);
        }
        assert_eq!(pd.n_events_in, 0);
        pd.push_note_on(0, 60, 1.0, 0, 1);
        assert_eq!(pd.n_events_in, 1);
    }
}
