use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::shmem::NamedShmem;

/// (A1 r.md #8) フォールバック既定サンプルレート。 通常はランタイムで
/// `AudioSession.sample_rate` = デバイス実レート (daw_audio が Hello で報告) が
/// SSoT で、 この const はデバイス query 失敗時の保険値としてのみ使う。
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;
/// 1 バッファの最大 frame 数。 SSoT は `process_data::MAX_FRAMES` (プラグイン
/// process shmem のバッファ次元) — audio bridge 側の u32 view として re-export。
/// 二重定義で乖離すると RT パスの `assert!(frames <= MAX_FRAMES)` が panic する。
pub const MAX_FRAMES: u32 = crate::process_data::MAX_FRAMES as u32;
pub const CHANNELS: u32 = 2;
/// Hard cap for the per-track peak meter ring in shmem. Tracks beyond this
/// index are still processed, they just don't publish a meter. 32 matches
/// Renoise's default Mixer column count.
pub const MAX_TRACKS: usize = 32;
/// docs/plan_modulation.md §4.2: hard cap for the modulation-scalar ring in
/// shmem. `Song::mod_sources` beyond this index don't publish a scalar (their
/// `EnvelopeFollow` node isn't emitted). Indexed by `ModSource` position.
pub const MAX_MOD_SOURCES: usize = 64;

/// Shared memory telemetry plane: daw_audio (writer) → daw_gui (30Hz
/// polling reader)。
///
/// `playhead_samples` is published by **daw_audio** at the end of every
/// buffer so daw_gui can poll it (once per UI tick) for playhead-row
/// highlighting.
///
/// `peak_l` / `peak_r` are the most recent per-block peaks (linear
/// amplitude, stored as `f32::to_bits`) written by daw_audio after applying
/// master gain, so daw_gui can draw a level meter. All fields are
/// lock-free Acquire/Release atomics — readers tolerate any value they
/// happen to observe.
///
/// v29 (`docs/plan_arch_refactor.md` §2): 旧 `frames_requested` / `samples`
/// 面 (M0 時代の request/ready セマフォ往復データプレーン) は writer /
/// reader とも存在しない死んだ protocol だったため削除。音声データは
/// per-plugin の `ProcessData` shmem + `WorkerBridge` dispatch が運ぶ。
#[repr(C)]
pub struct AudioBridge {
    pub playhead_samples: AtomicU64,
    pub peak_l: AtomicU32,
    pub peak_r: AtomicU32,
    /// Per-track post-fader peaks, `[track][0=L, 1=R]`, as `f32::to_bits`.
    /// Written by **daw_audio** after summing each track into the master
    /// bus (`engine.rs` / `main.rs` の `set_track_peak`); read by daw_gui on
    /// its UI tick。(v29: 旧 daw_plugin_host request/ready data plane 撤去に伴い
    /// writer は daw_audio に一本化 — module doc 参照。)
    pub track_peaks: [[AtomicU32; 2]; MAX_TRACKS],
    /// docs/plan_modulation.md §4.2: per-`ModSource` envelope follower scalar
    /// (`f32::to_bits`), block-rate. Written by the audio engine every buffer
    /// (`env[frames-1]` of the source's follower), polled by the GUI at ~30Hz
    /// alongside `track_peaks` and applied to modulated params. Indexed by
    /// `ModSource` position in `Song::mod_sources` (= `EnvelopeFollow::slot`).
    pub mod_scalars: [AtomicU32; MAX_MOD_SOURCES],
    /// Phase 7 B4 Step C (2026-05-13): count-in 残り samples mirror (audio
    /// thread が `process_buffer` で書く、 GUI が on_tick で poll)。
    /// 0 = count-in 中ではない / 完了済。 `StartRecording` 受信時に audio
    /// thread が値を立てる。 **これ単体で「count-in が終わったか」を判定しては
    /// いけない** — 0 は「まだ始まっていない」も意味するので、録音実体の開始判定は
    /// [`AudioBridge::recording_live`] を見る (r.md #51)。
    pub preroll_remaining_samples: AtomicU64,
    /// r.md #51: engine が今 transport を回しているか (0/1)。
    ///
    /// **「再生中か」の唯一の所有者は engine** で、GUI はこれを観測して
    /// `transport.is_playing` に写すだけ。GUI 側で「Play を送った記憶」を持つと、
    /// engine が自分で止まったとき (曲末 auto-stop / 書き出し) に食い違う。
    pub playing: AtomicU32,
    /// r.md #51: 今この瞬間 MIDI ノートを記録してよいか (0/1)。
    ///
    /// `録音要求あり && 再生中 && count-in 完了` を engine が判定して publish する。
    /// GUI が preroll ミラーの 0 を見て自前で導出すると、`StartRecording` 送信直後に
    /// 届いた stale な Tick (まだ preroll が立つ前の 0) で count-in を丸ごと飛ばす。
    pub recording_live: AtomicU32,
}

impl AudioBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Owning handle to the audio shared memory region.
pub struct AudioBridgeHandle {
    shmem: NamedShmem,
}

impl AudioBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, AudioBridge::SIZE)?;
        // Zero-initialize so the AtomicU32 starts at 0 and samples are silent.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, AudioBridge::SIZE) };
        let handle = Self { shmem };
        // Publish the "not playing" sentinel before any reader polls, so the
        // GUI highlight is off until plugin_host announces a real playhead.
        handle.set_playhead_samples(u64::MAX);
        Ok(handle)
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, AudioBridge::SIZE)?;
        Ok(Self { shmem })
    }

    fn ptr(&self) -> *mut AudioBridge {
        self.shmem.as_ptr() as *mut AudioBridge
    }

    pub fn bridge(&self) -> &AudioBridge {
        unsafe { &*self.ptr() }
    }

    pub fn set_playhead_samples(&self, n: u64) {
        self.bridge().playhead_samples.store(n, Ordering::Release);
    }

    pub fn playhead_samples(&self) -> u64 {
        self.bridge().playhead_samples.load(Ordering::Acquire)
    }

    pub fn set_peaks(&self, l: f32, r: f32) {
        self.bridge()
            .peak_l
            .store(l.to_bits(), Ordering::Release);
        self.bridge()
            .peak_r
            .store(r.to_bits(), Ordering::Release);
    }

    pub fn peaks(&self) -> (f32, f32) {
        let l = f32::from_bits(self.bridge().peak_l.load(Ordering::Acquire));
        let r = f32::from_bits(self.bridge().peak_r.load(Ordering::Acquire));
        (l, r)
    }

    /// Publishes one track's post-fader peak pair. Out-of-range track
    /// indices (beyond `MAX_TRACKS`) are silently dropped — the track is
    /// still mixed, it just doesn't get a meter.
    pub fn set_track_peak(&self, track: usize, l: f32, r: f32) {
        let Some(slot) = self.bridge().track_peaks.get(track) else {
            return;
        };
        slot[0].store(l.to_bits(), Ordering::Release);
        slot[1].store(r.to_bits(), Ordering::Release);
    }

    pub fn track_peak(&self, track: usize) -> (f32, f32) {
        let Some(slot) = self.bridge().track_peaks.get(track) else {
            return (0.0, 0.0);
        };
        let l = f32::from_bits(slot[0].load(Ordering::Acquire));
        let r = f32::from_bits(slot[1].load(Ordering::Acquire));
        (l, r)
    }

    /// Fills `out` with `(L, R)` peaks for tracks 0..`out.len()`.
    /// Out-of-range tracks are reported as `(0.0, 0.0)`.
    /// Phase 7 B4 Step C: count-in 残り samples を audio thread が更新。
    /// `StartRecording` 受信時に audio thread が preroll を立て、
    /// `process_buffer` が preroll > 0 ループ内で毎 buffer 更新する。
    /// 0 到達で通常再生に戻る。
    pub fn set_preroll_remaining(&self, n: u64) {
        self.bridge()
            .preroll_remaining_samples
            .store(n, Ordering::Release);
    }

    pub fn preroll_remaining(&self) -> u64 {
        self.bridge()
            .preroll_remaining_samples
            .load(Ordering::Acquire)
    }

    /// r.md #51: engine の transport 走行状態を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。読み手は GUI の playhead poller。
    pub fn set_playing(&self, playing: bool) {
        self.bridge()
            .playing
            .store(u32::from(playing), Ordering::Release);
    }

    pub fn playing(&self) -> bool {
        self.bridge().playing.load(Ordering::Acquire) != 0
    }

    /// r.md #51: 「今ノートを記録してよいか」を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。count-in 明けの立ち上がりもここが唯一の合図。
    pub fn set_recording_live(&self, live: bool) {
        self.bridge()
            .recording_live
            .store(u32::from(live), Ordering::Release);
    }

    pub fn recording_live(&self) -> bool {
        self.bridge().recording_live.load(Ordering::Acquire) != 0
    }

    pub fn track_peaks(&self, out: &mut Vec<(f32, f32)>) {
        out.clear();
        for i in 0..MAX_TRACKS {
            let slot = &self.bridge().track_peaks[i];
            let l = f32::from_bits(slot[0].load(Ordering::Acquire));
            let r = f32::from_bits(slot[1].load(Ordering::Acquire));
            out.push((l, r));
        }
    }

    /// docs/plan_modulation.md §4.2: publish one `ModSource`'s envelope
    /// follower scalar. Out-of-range slots (beyond `MAX_MOD_SOURCES`) are
    /// silently dropped. RT-safe atomic store.
    pub fn set_mod_scalar(&self, slot: usize, v: f32) {
        let Some(cell) = self.bridge().mod_scalars.get(slot) else {
            return;
        };
        cell.store(v.to_bits(), Ordering::Release);
    }

    pub fn mod_scalar(&self, slot: usize) -> f32 {
        let Some(cell) = self.bridge().mod_scalars.get(slot) else {
            return 0.0;
        };
        f32::from_bits(cell.load(Ordering::Acquire))
    }

    /// Fills `out` with the modulation scalars for slots `0..MAX_MOD_SOURCES`.
    pub fn mod_scalars(&self, out: &mut Vec<f32>) {
        out.clear();
        for i in 0..MAX_MOD_SOURCES {
            out.push(f32::from_bits(
                self.bridge().mod_scalars[i].load(Ordering::Acquire),
            ));
        }
    }
}

// The underlying shared memory is safe to share across threads; every
// field is a lock-free atomic and readers tolerate any observed value.
unsafe impl Send for AudioBridgeHandle {}
unsafe impl Sync for AudioBridgeHandle {}

pub fn shmem_id(parent_pid: u32) -> String {
    format!("daw_01_audio_{parent_pid}")
}
