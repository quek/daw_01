use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result};
use shared_memory::{Shmem, ShmemConf};

/// (A1 r.md #8) フォールバック既定サンプルレート。 通常はランタイムで
/// `AudioSession.sample_rate` = デバイス実レート (daw_audio が Hello で報告) が
/// SSoT で、 この const はデバイス query 失敗時の保険値としてのみ使う。
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;
/// 1 バッファの最大 frame 数。 SSoT は `process_data::MAX_FRAMES` (プラグイン
/// process shmem のバッファ次元) — audio bridge 側の u32 view として re-export。
/// 二重定義で乖離すると RT パスの `assert!(frames <= MAX_FRAMES)` が panic する。
pub const MAX_FRAMES: u32 = crate::process_data::MAX_FRAMES as u32;
pub const CHANNELS: u32 = 2;
pub const SAMPLE_BUFFER_LEN: usize = (MAX_FRAMES * CHANNELS) as usize;
/// Hard cap for the per-track peak meter ring in shmem. Tracks beyond this
/// index are still processed, they just don't publish a meter. 32 matches
/// Renoise's default Mixer column count.
pub const MAX_TRACKS: usize = 32;
/// docs/plan_modulation.md §4.2: hard cap for the modulation-scalar ring in
/// shmem. `Song::mod_sources` beyond this index don't publish a scalar (their
/// `EnvelopeFollow` node isn't emitted). Indexed by `ModSource` position.
pub const MAX_MOD_SOURCES: usize = 64;

/// Shared memory layout between daw_plugin_host (writer), daw_audio (reader +
/// meter writer) and daw_gui (polling reader).
/// daw_audio populates `frames_requested` then signals the request semaphore;
/// daw_plugin_host fills `samples` (interleaved stereo) then signals the ready
/// semaphore.
///
/// `playhead_samples` is published by daw_plugin_host at the end of every
/// buffer so daw_gui can poll it (once per UI tick) for playhead-row
/// highlighting.
///
/// `peak_l` / `peak_r` are the most recent per-block peaks (linear
/// amplitude, stored as `f32::to_bits`) written by daw_audio after applying
/// master gain, so daw_gui can draw a level meter. All three auxiliary
/// fields are lock-free Acquire/Release atomics — readers tolerate any
/// value they happen to observe.
#[repr(C)]
pub struct AudioBridge {
    pub frames_requested: AtomicU32,
    _pad: u32,
    pub playhead_samples: AtomicU64,
    pub peak_l: AtomicU32,
    pub peak_r: AtomicU32,
    /// Per-track post-fader peaks, `[track][0=L, 1=R]`, as `f32::to_bits`.
    /// Written by daw_plugin_host after summing each track into the master
    /// bus; read by daw_gui on its UI tick.
    pub track_peaks: [[AtomicU32; 2]; MAX_TRACKS],
    /// docs/plan_modulation.md §4.2: per-`ModSource` envelope follower scalar
    /// (`f32::to_bits`), block-rate. Written by the audio engine every buffer
    /// (`env[frames-1]` of the source's follower), polled by the GUI at ~30Hz
    /// alongside `track_peaks` and applied to modulated params. Indexed by
    /// `ModSource` position in `Song::mod_sources` (= `EnvelopeFollow::slot`).
    pub mod_scalars: [AtomicU32; MAX_MOD_SOURCES],
    /// Phase 7 B4 Step C (2026-05-13): count-in 残り samples mirror (audio
    /// thread が `process_buffer` で書く、 GUI が on_tick で poll)。 GUI 側は
    /// `midi_recording_pending` 中だけ参照、 0 検出で `midi_recording` に昇格。
    /// 0 = count-in 中ではない / 完了済。 通常再生中は audio thread が更新
    /// しないので、 `StartCountIn` 受信時に audio thread が値を立てる。
    pub preroll_remaining_samples: AtomicU64,
    _pad_preroll: u32,
    pub samples: [f32; SAMPLE_BUFFER_LEN],
}

impl AudioBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Owning handle to the audio shared memory region.
pub struct AudioBridgeHandle {
    shmem: Shmem,
}

impl AudioBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = ShmemConf::new()
            .size(AudioBridge::SIZE)
            .os_id(os_id)
            .create()
            .with_context(|| format!("failed to create shmem {os_id}"))?;
        // Zero-initialize so the AtomicU32 starts at 0 and samples are silent.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, AudioBridge::SIZE) };
        let handle = Self { shmem };
        // Publish the "not playing" sentinel before any reader polls, so the
        // GUI highlight is off until plugin_host announces a real playhead.
        handle.set_playhead_samples(u64::MAX);
        Ok(handle)
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = ShmemConf::new()
            .os_id(os_id)
            .open()
            .with_context(|| format!("failed to open shmem {os_id}"))?;
        anyhow::ensure!(
            shmem.len() >= AudioBridge::SIZE,
            "shmem too small: {} < {}",
            shmem.len(),
            AudioBridge::SIZE
        );
        Ok(Self { shmem })
    }

    fn ptr(&self) -> *mut AudioBridge {
        self.shmem.as_ptr() as *mut AudioBridge
    }

    pub fn bridge(&self) -> &AudioBridge {
        unsafe { &*self.ptr() }
    }

    pub fn samples_ptr(&self) -> *mut f32 {
        let bridge = self.ptr();
        unsafe { (&raw mut (*bridge).samples) as *mut f32 }
    }

    pub fn set_frames_requested(&self, n: u32) {
        self.bridge().frames_requested.store(n, Ordering::Release);
    }

    pub fn frames_requested(&self) -> u32 {
        self.bridge().frames_requested.load(Ordering::Acquire)
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
    /// `StartCountIn` 受信時は GUI 側の `app.rs` が `MainToChild` 経由で
    /// 流す → audio thread の `process_buffer` が preroll > 0 ループ内で
    /// 毎 buffer `set_preroll_remaining(new_value)` を呼ぶ。 0 到達で
    /// 通常再生に戻る。
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

// The underlying shared memory is safe to share across threads; the single
// atomic counter and the sample buffer are protected by the request/ready
// semaphore handshake.
unsafe impl Send for AudioBridgeHandle {}
unsafe impl Sync for AudioBridgeHandle {}

pub fn shmem_id(parent_pid: u32) -> String {
    format!("daw_01_audio_{parent_pid}")
}

pub fn request_sem_id(parent_pid: u32) -> String {
    format!("daw_01_req_{parent_pid}")
}

pub fn ready_sem_id(parent_pid: u32) -> String {
    format!("daw_01_ready_{parent_pid}")
}
