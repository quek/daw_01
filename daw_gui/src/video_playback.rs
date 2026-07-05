//! Playback-time video frame decode (`docs/plan_video.md` P5).
//!
//! Synchronous (= no background thread) decoder driven each frame
//! from `Runner::render_frame`. Holds per-`VideoSourceId`
//! `IMFSourceReader` so sequential ReadSample (= playback) is cheap;
//! seeks only when the playhead jumps (= scrub / transport move /
//! play-from-start). Returns RGBA8 bytes that `Runner` uploads via
//! `Renderer::upload_texture_rgba` into a single reusable preview
//! texture.
//!
//! Multi-clip composite (= crossfade + multi-track) is P7. P5 covers
//! "show the frame for the topmost active video clip at the
//! playhead" only — exactly mirroring the REAPER preview behaviour
//! when no video FX chain is present.

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use common::model::{FadeCurve, Song, VideoEvent, VideoSourceId};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource1};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MFVideoFormat_ARGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, Interface, PCWSTR};

/// `HANDLE` (= `*mut c_void` newtype) is not `Send` by default. We
/// transport DXGI shared NT handles from the decode worker to the
/// main render thread via `mpsc::channel`, which requires `Send`.
/// Wrap once at the channel boundary; both sides treat the value as
/// an opaque kernel object — it is safe to move because the kernel
/// owns the underlying resource, not the Rust thread.
#[derive(Clone, Copy, Debug)]
pub struct SendHandle(pub HANDLE);

// SAFETY: `HANDLE` wraps a `HANDLE` (= `*mut c_void`) which is just a
// kernel object identifier; it has no thread affinity, only the
// referenced D3D11 resource does (and that resource lives in the
// shared GPU memory pool, not in either thread's heap).
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

/// docs/plan_video_perf.md P4: ring buffer size. 6 frames at 30fps =
/// 200ms of lookahead, which absorbs the typical 4-6 frame burst from
/// `IMFSourceReader::ReadSample` returning a contiguous run of decoded
/// samples after a single HW decode kick. Each `VideoSourceId` owns
/// `PREVIEW_RING_SIZE` independent D3D11 destination textures (=
/// `SharedPool::slots`) so the worker can write into slot N+1 without
/// invalidating the wgpu sample of slot N that the main thread is
/// presenting this frame.
pub const PREVIEW_RING_SIZE: usize = 6;

/// Default forward-walk budget (µs) for `decode_at`: a target within this
/// window of the last-decoded frame is reached by `ReadSample`-ing forward
/// (cheap) rather than seeking. Ring slot 0 always uses this; slots 1..N may
/// pass a larger budget so a low-fps source (step > this) forward-walks the
/// single frame from the previous slot instead of re-seeking per slot.
pub const DEFAULT_FORWARD_BUDGET_MICROS: u64 = 100_000;

/// One decoded frame, ready for the main thread to push into the
/// preview scene. Two underlying paths:
///
/// - **`Shared`** (zero-copy, P3): the decoded BGRA frame lives in a
///   D3D11 texture that was created with `SHARED_NTHANDLE +
///   SHARED_KEYEDMUTEX`. The worker calls `IDXGIResource1::CreateSharedHandle`
///   once per `(source_id, slot_idx)` and reuses the same handle for
///   every subsequent frame that lands in that slot; the main thread
///   calls `Renderer::create_texture_from_d3d11_shared_handle` exactly
///   once per `(source_id, slot_idx)` and reuses the resulting
///   `TextureHandle`. The pixel bytes never touch CPU memory.
/// - **`Bgra`** (P2 fallback): the decode produced system-memory
///   pixels (= the source did not use the HW decoder path even with
///   `MF_SOURCE_READER_D3D_MANAGER` attached, or `try_init_d3d11`
///   failed). Main thread uploads via `Renderer::upload_texture_bgra`.
///   Ring buffering is disabled for this path (slot_idx is always 0);
///   the fallback exists for HW-less environments where pacing matters
///   less than basic playback.
#[derive(Debug, Clone)]
pub enum DecodedFrame {
    Shared {
        width: u32,
        height: u32,
        /// DXGI shared NT handle of the `SharedSlot` this frame was
        /// written into. Stable for the lifetime of the `(source_id,
        /// slot_idx)` pair — main thread imports each unique slot
        /// exactly once into wgpu.
        handle: SendHandle,
        /// Index into `SharedPool::slots` that this frame occupies. The
        /// worker round-robins through `0..PREVIEW_RING_SIZE` when
        /// building a ring snapshot; the main thread keys its
        /// `(VideoSourceId, slot_idx)` texture cache off this so each
        /// slot's handle is imported into wgpu exactly once.
        slot_idx: u8,
    },
    Bgra {
        width: u32,
        height: u32,
        /// Tightly-packed BGRA8 in scanline order, length = `width * height * 4`.
        bgra: Vec<u8>,
    },
}

impl DecodedFrame {
    pub fn width(&self) -> u32 {
        match self {
            Self::Shared { width, .. } | Self::Bgra { width, .. } => *width,
        }
    }
    pub fn height(&self) -> u32 {
        match self {
            Self::Shared { height, .. } | Self::Bgra { height, .. } => *height,
        }
    }
}

/// One video clip active at the current playhead. The runner walks
/// the returned list bottom-up (= `z_index` ascending) and pushes one
/// textured quad per layer with the per-event `alpha`; gui_01's
/// call-order interleave then blends them via standard "src OVER
/// dst" semantics (= top track wins when alpha=1, crossfade
/// midpoint mixes at alpha=0.5/0.5). v12 (`docs/plan_video.md` §4 P7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveVideoFrame {
    pub video_source_id: VideoSourceId,
    /// この動画フレームを持つ track id (映像効果チェーンの解決に使う、
    /// `ActiveImageFrame::owning_track_id` と対)。
    pub owning_track_id: u32,
    pub source_micros: u64,
    /// Per-event alpha derived from `fade_in_beats` / `fade_out_beats`
    /// and the matching curve. `1.0` is fully opaque, `0.0` invisible.
    /// Caller pushes the quad directly with this value.
    pub alpha: f32,
    /// Bottom-up draw order. `0` is the lowest video track (drawn
    /// first), each higher track increments. Within the same track
    /// adjacent / overlapping events share the same `z_index`; their
    /// individual `alpha`s do the crossfade.
    pub z_index: u32,
}

/// One `IMFSourceReader` plus a few cached attributes that don't
/// change after the reader is built.
struct ReaderEntry {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    /// μs of the most recent frame we decoded, in source time. Used
    /// to decide whether the next `decode_at` is a forward-step (=
    /// just keep ReadSample-ing) or a jump (= SetCurrentPosition
    /// seek + flush).
    last_decoded_micros: Option<u64>,
    /// Decrement-on-decode counter. While > 0 the engine emits one
    /// `tracing::info!` line per decode with `walk_ms` / `swap_ms`
    /// to expose whether WMF picked hardware or software H.264
    /// decode (HW ≈ 5-10ms / frame, SW ≈ 30-80ms / frame at 1080p).
    /// **Only decrements when `target_micros > 0`** so the budget
    /// goes to real playback rather than the idle-at-beat-0
    /// repeated-request pattern (which would otherwise eat the
    /// warm-up window before Play is even pressed).
    timing_log_remaining: u32,
    /// docs/plan_video_perf.md P4: per-source pool of
    /// `PREVIEW_RING_SIZE` independent DXGI-shared destination textures
    /// for the zero-copy ring buffer path. Created on first HW-decoded
    /// sample (= when the reader was built with
    /// `MF_SOURCE_READER_D3D_MANAGER` attached and `try_init_d3d11`
    /// succeeded). The worker round-robins through `slots[slot_idx]`
    /// when filling a ring snapshot; the main thread imports each
    /// slot's handle into wgpu exactly once.
    shared_pool: Option<SharedPool>,
}

/// One slot in a per-source ring buffer of D3D11-backed destination
/// textures. Each slot is independent (= its own NT handle, its own
/// keyed mutex, its own texture) so the worker can be writing into
/// slot N+1 while wgpu is sampling slot N for the present pass —
/// without the slot's handle being "the latest" requiring any sync.
struct SharedSlot {
    /// Our owned D3D11 texture, created with `SHARED_NTHANDLE +
    /// SHARED_KEYEDMUTEX`. The WMF decoder writes into a texture from
    /// its own pool; we `CopySubresourceRegion` from that into this
    /// shared destination on each frame.
    texture: ID3D11Texture2D,
    /// `IDXGIKeyedMutex` view of `texture`. Worker acquires before
    /// `CopySubresourceRegion`, releases after.
    ///
    /// **DO NOT REMOVE** these calls thinking they're "dead because
    /// the main thread doesn't pair with them" — verified empirically
    /// (regression commit `c2ae697`, reverted by `6b5eebd`, 2026-05-26)
    /// that wgpu's DX12 / Vulkan import side **internally consumes**
    /// the keyed-mutex protocol when sampling the imported texture.
    /// Removing the worker's Acquire(0) / Release(0) pair leaves the
    /// mutex perpetually "held by worker" from wgpu's perspective,
    /// and the imported texture renders as fully transparent (= the
    /// "preview is blank dark backdrop only" bug). The daw_01 main
    /// thread does NOT need to call mutex APIs itself.
    mutex: IDXGIKeyedMutex,
    /// Cached NT handle from `IDXGIResource1::CreateSharedHandle`.
    /// Stable across the slot's lifetime — wrapped in `SendHandle`
    /// when it leaves the worker thread.
    handle: HANDLE,
}

impl Drop for SharedSlot {
    fn drop(&mut self) {
        // (review) `CreateSharedHandle` の NT handle は COM 参照カウントと独立で、
        // owner が CloseHandle しない限りプロセス生存中リークし、 参照先 texture の
        // GPU メモリもピン留めされ得る (MF→libav fallback で `readers.remove()`
        // されたときに顕在)。 wgpu の import 側は open 時に独自参照を持つので、
        // ここで閉じても imported texture は無効にならない。 texture / mutex の
        // COM 解放 (自動 Drop) とは独立に handle だけ閉じる。
        if !self.handle.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

/// Per-source pool of `PREVIEW_RING_SIZE` `SharedSlot`s. Allocated
/// lazily on first HW decode (= `write_to_shared_pool`), reused for
/// every subsequent frame for the same `VideoSourceId`. Dropped only
/// when the engine itself is dropped (= process exit for MVP).
///
/// docs/plan_video_perf.md P4: the ring buffer's storage layer. The
/// worker writes into `slots[slot_idx]` in round-robin order; the
/// main thread keys its `(VideoSourceId, slot_idx)` `TextureHandle`
/// cache off the slot index so each slot's handle is imported into
/// wgpu exactly once across the source's lifetime.
struct SharedPool {
    slots: [SharedSlot; PREVIEW_RING_SIZE],
}

/// docs/plan_video_perf.md P1: process-wide D3D11 device + IMFDXGIDevice
/// manager that backs WMF's hardware H.264 decoder. Created lazily on
/// the worker thread (= first `decode_at` call) so test threads that
/// never touch playback don't pay the device-create cost; created once
/// and shared across every `ReaderEntry` so all readers see the same
/// `IMFDXGIDeviceManager`.
struct D3D11WmfState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
}

/// Create a feature-level-11.1 D3D11 device suitable for WMF video
/// decode (= `VIDEO_SUPPORT` + `BGRA_SUPPORT`), wrap it in an
/// `IMFDXGIDeviceManager`, and mark the device multithread-protected.
/// Returns `Err` when no GPU is available (= the WMF SW fallback path
/// still works without this — caller treats the failure as "stay on
/// SW decode" rather than abort).
fn try_init_d3d11() -> Result<D3D11WmfState, String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // Order matters: feature_level_11_1 first so newer drivers pick the
    // best path; 11.0 is the fallback (= every D3D11-capable GPU since
    // 2012 supports either).
    let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None, // pAdapter — default adapter
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    }
    let device = device.ok_or_else(|| "D3D11 device null".to_string())?;
    let context = context.ok_or_else(|| "D3D11 immediate context null".to_string())?;

    // WMF documentation requires the D3D11 device used through an
    // IMFDXGIDeviceManager to be MultithreadProtected (= the decoder
    // may issue draw calls from a worker thread inside MF). Without
    // this, IMFDXGIDeviceManager::LockDevice on subsequent threads can
    // race against ReadSample.
    unsafe {
        let multithread: ID3D10Multithread = device
            .cast()
            .map_err(|e| format!("ID3D10Multithread cast: {e}"))?;
        let _ = multithread.SetMultithreadProtected(true);
    }

    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe {
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
            .map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
    }
    let manager = manager.ok_or_else(|| "DXGI device manager null".to_string())?;
    unsafe {
        manager
            .ResetDevice(&device, reset_token)
            .map_err(|e| format!("IMFDXGIDeviceManager::ResetDevice: {e}"))?;
    }

    Ok(D3D11WmfState {
        device,
        context,
        manager,
    })
}

/// Stateful playback decoder. Owned by `Runner` for the lifetime of
/// the process. Readers are created lazily on first request per
/// VideoSourceId; the engine never tears them down on its own (=
/// project unload happens at process exit for MVP).
pub struct VideoPlaybackEngine {
    readers: HashMap<VideoSourceId, ReaderEntry>,
    /// docs/plan_video_perf.md P1: lazy-init D3D11 / DXGI device manager
    /// for HW H.264 decode. `None` = either not yet initialized, or
    /// init failed (= fall through to SW decode path, same code paths
    /// as before P1). Set on the worker thread by `decode_at`.
    d3d11: Option<D3D11WmfState>,
    /// One-shot flag: once `decode_at` has tried to init D3D11 (success
    /// or failure), don't retry per call. Failure path emits one
    /// warning then proceeds with SW decode for the rest of the
    /// process lifetime.
    d3d11_init_attempted: bool,
    /// In-process libav software decoder (`docs/plan_video_export_libav.md`
    /// Phase 3). Sources the Media Foundation decoder can't handle (e.g.
    /// 10-bit H.264 High10) are switched to this for the rest of the session
    /// (CPU/BGRA → `DecodedFrame::Bgra`) — no `ffmpeg.exe` subprocess.
    libav_fallback: crate::libav_decoder::LibavVideoDecoder,
    /// Source ids that have fallen back to `libav_fallback`.
    libav_fallback_ids: std::collections::HashSet<VideoSourceId>,
}

impl VideoPlaybackEngine {
    pub fn new() -> Self {
        Self {
            readers: HashMap::new(),
            d3d11: None,
            d3d11_init_attempted: false,
            libav_fallback: crate::libav_decoder::LibavVideoDecoder::new(),
            libav_fallback_ids: std::collections::HashSet::new(),
        }
    }

    /// Pure helper: walk the song bottom-up and return every video
    /// clip active at `playhead_beat`, with a per-event `alpha`
    /// derived from the clip's `fade_in_beats` / `fade_out_beats` (=
    /// MVP crossfade behaviour). Returns a `Vec<ActiveVideoFrame>`
    /// ordered from lowest to topmost track so the caller can
    /// composite by call-order (= last pushed quad ends up on top).
    /// v12 (`docs/plan_video.md` §4 P7).
    ///
    /// Muted events are dropped from the result entirely (= same as
    /// the singular `active_source_at` MVP behaviour).
    pub fn active_sources_at(song: &Song, playhead_beat: f64) -> Vec<ActiveVideoFrame> {
        let bpm = song.bpm as f64;
        if bpm <= 0.0 {
            return Vec::new();
        }
        // A4 (r.md #8): tempo automation がある曲は映像 source 時間 (秒) を tempo
        // 積分で求める (= 映像が音とズレない)。 constant 曲は従来の高速 60/bpm 経路。
        let tempo_map = song_tempo_map_if_automated(song);
        let playhead_secs = tempo_map.as_ref().map(|m| m.beat_to_seconds(playhead_beat));
        let mut out: Vec<ActiveVideoFrame> = Vec::new();
        // `song.tracks[0]` is the top of the arrangement, so iterating
        // `.rev()` yields bottom-most → topmost. Each video track gets
        // a contiguous `z_index` counter so events on the same track
        // share a layer (= their alphas blend within layer instead of
        // creating a third layer between clip A and clip B during
        // crossfade).
        let mut z_index: u32 = 0;
        for track in song.tracks.iter().rev() {
            // v16: TrackKind 廃止後は「video_events を持つ clip がある
            // track」 が visual composite に参加する (= filter は content
            // kind で行う、 後段 `content.video_events()` で None を skip)。
            // 自身の mute だけでなく、 グループ親の mute / solo (audio と同じ
            // effective-mute) で silenced な track は preview / render の両方で
            // skip する (`Song::track_visually_silenced` が SSoT)。
            if song.track_visually_silenced(track.id) {
                continue;
            }
            let mut track_emitted = false;
            for clip in &track.clips {
                // muted clip は video composite から除外する (黒/下層が出る)。
                if clip.muted {
                    continue;
                }
                let clip_start = clip.start_beat;
                let clip_end = clip.start_beat + clip.length_beats;
                if playhead_beat < clip_start || playhead_beat >= clip_end {
                    continue;
                }
                let clip_local = playhead_beat - clip_start;
                let Some(content) = song.clip_contents.get(&clip.content_id) else {
                    continue;
                };
                let Some(events) = content.video_events() else {
                    continue;
                };
                for event in events {
                    let event_end =
                        event.event_start_in_clip_beats + event.event_length_beats;
                    if clip_local < event.event_start_in_clip_beats
                        || clip_local >= event_end
                    {
                        continue;
                    }
                    if event.muted {
                        continue;
                    }
                    let event_progress_beats =
                        clip_local - event.event_start_in_clip_beats;
                    let event_progress_secs = match (&tempo_map, playhead_secs) {
                        (Some(m), Some(ph_secs)) => {
                            let event_start = clip_start + event.event_start_in_clip_beats;
                            (ph_secs - m.beat_to_seconds(event_start)).max(0.0)
                        }
                        _ => event_progress_beats * 60.0 / bpm,
                    };
                    let event_progress_micros =
                        (event_progress_secs * 1_000_000.0).round() as u64;
                    let source_micros = event
                        .source_start_micros
                        .saturating_add(event_progress_micros)
                        .min(event.source_end_micros);
                    let alpha = event_alpha(event, clip_local);
                    if alpha <= 0.0 {
                        continue;
                    }
                    out.push(ActiveVideoFrame {
                        video_source_id: event.source_id,
                        owning_track_id: track.id,
                        source_micros,
                        alpha,
                        z_index,
                    });
                    track_emitted = true;
                }
            }
            if track_emitted {
                z_index += 1;
            }
        }
        out
    }

    /// Backwards-compatible singular accessor (`docs/plan_video.md`
    /// P5 baseline). Equivalent to `active_sources_at(...).last()` —
    /// the topmost active layer wins, with its alpha taken into
    /// account so a faded-out top clip with alpha < threshold defers
    /// to whatever is underneath. Kept around for callers that don't
    /// composite (= e.g. arrangement thumbnail picker).
    pub fn active_source_at(
        song: &Song,
        playhead_beat: f64,
    ) -> Option<(VideoSourceId, u64)> {
        let bpm = song.bpm as f64;
        if bpm <= 0.0 {
            return None;
        }
        // A4 (r.md #8): tempo automation 時は映像 source 時間を tempo 積分で求める。
        let tempo_map = song_tempo_map_if_automated(song);
        let playhead_secs = tempo_map.as_ref().map(|m| m.beat_to_seconds(playhead_beat));
        for track in &song.tracks {
            // v16: TrackKind 廃止後は「video_events を持つ clip がある
            // track」 が visual composite に参加する。 `content.video
            // _events()` で None を skip する経路に統合。
            if track.muted {
                continue;
            }
            for clip in &track.clips {
                // muted clip は video composite から除外する。
                if clip.muted {
                    continue;
                }
                let clip_start = clip.start_beat;
                let clip_end = clip.start_beat + clip.length_beats;
                if playhead_beat < clip_start || playhead_beat >= clip_end {
                    continue;
                }
                let clip_local = playhead_beat - clip_start;
                let Some(content) = song.clip_contents.get(&clip.content_id) else {
                    continue;
                };
                let Some(events) = content.video_events() else {
                    continue;
                };
                for event in events {
                    let event_end =
                        event.event_start_in_clip_beats + event.event_length_beats;
                    if clip_local < event.event_start_in_clip_beats
                        || clip_local >= event_end
                    {
                        continue;
                    }
                    if event.muted {
                        return None;
                    }
                    let event_progress_beats =
                        clip_local - event.event_start_in_clip_beats;
                    let event_progress_secs = match (&tempo_map, playhead_secs) {
                        (Some(m), Some(ph_secs)) => {
                            let event_start = clip_start + event.event_start_in_clip_beats;
                            (ph_secs - m.beat_to_seconds(event_start)).max(0.0)
                        }
                        _ => event_progress_beats * 60.0 / bpm,
                    };
                    let event_progress_micros =
                        (event_progress_secs * 1_000_000.0).round() as u64;
                    let source_micros = event
                        .source_start_micros
                        .saturating_add(event_progress_micros)
                        .min(event.source_end_micros);
                    return Some((event.source_id, source_micros));
                }
            }
        }
        None
    }

    /// Decode the frame at `target_micros`. Tries the Media Foundation
    /// path first (HW D3D11 zero-copy for 8-bit); if MF cannot decode the
    /// source (e.g. 10-bit H.264 High10, which the stock MS H.264 decoder
    /// rejects with `CopyDecodedFrame failed (0x80004005)`), this source is
    /// switched to the in-process libav software decoder for the rest of the
    /// session (CPU/BGRA → `DecodedFrame::Bgra`) — no `ffmpeg.exe` subprocess.
    pub fn decode_at(
        &mut self,
        video_source_id: VideoSourceId,
        source_path: &Path,
        target_micros: u64,
        slot_idx: u8,
        forward_budget_micros: u64,
    ) -> Result<DecodedFrame, String> {
        if self.libav_fallback_ids.contains(&video_source_id) {
            // The libav fallback is CPU/BGRA; the runner collapses every ring
            // slot into the single `(source_id, 0)` texture (1-frame-latest),
            // so only the center slot is meaningful. Signal "no more" for
            // slots > 0 so the worker truncates the ring to the playhead.
            if slot_idx != 0 {
                return Err("libav fallback: center slot only".to_string());
            }
            let f = self
                .libav_fallback
                .decode_at(video_source_id, source_path, target_micros)?;
            return Ok(DecodedFrame::Bgra { width: f.width, height: f.height, bgra: f.bgra });
        }
        match self.decode_at_mf(
            video_source_id,
            source_path,
            target_micros,
            slot_idx,
            forward_budget_micros,
        ) {
            Ok(frame) => Ok(frame),
            Err(mf_err) => {
                // MF can't decode this codec/profile (e.g. 10-bit High10).
                // Drop the MF reader and serve this source from the in-process
                // libav software decoder for the rest of the session. Unlike
                // the old ffmpeg.exe path this needs no dimensions up front
                // (libav reports them from the decoded frame), so it works even
                // when MF couldn't build a reader at all — the export 10-bit
                // "black frame" bug.
                self.readers.remove(&video_source_id);
                self.libav_fallback_ids.insert(video_source_id);
                tracing::info!(
                    video_source_id,
                    error = %mf_err,
                    "MF decode failed; switching source to in-process libav decoder"
                );
                if slot_idx != 0 {
                    return Err("libav fallback: center slot only".to_string());
                }
                let f = self
                    .libav_fallback
                    .decode_at(video_source_id, source_path, target_micros)
                    .map_err(|fe| {
                        format!("MF decode failed ({mf_err}); libav fallback also failed: {fe}")
                    })?;
                Ok(DecodedFrame::Bgra { width: f.width, height: f.height, bgra: f.bgra })
            }
        }
    }

    /// Decode (or just-fetch when the target lands on the same frame
    /// we last decoded) the frame at `target_micros` of the source.
    /// `source_path` is only consulted when the reader for this
    /// `VideoSourceId` hasn't been created yet — caller resolves the
    /// `VideoSourcePath` (ProjectRelative vs Absolute) before passing
    /// in.
    ///
    /// docs/plan_video_perf.md P4: `slot_idx` is the worker's
    /// round-robin index into the per-source `SharedPool` (=
    /// `0..PREVIEW_RING_SIZE`). On the HW path the resulting
    /// `DecodedFrame::Shared` carries this index back so the main
    /// thread can key its `(VideoSourceId, slot_idx)` texture cache.
    /// On the Bgra fallback path `slot_idx` is preserved by the
    /// returned `DecodedFrame::Bgra`'s outer position in the worker's
    /// ring (= the variant itself has no slot field; the ring entry
    /// preserves order so the main thread sees a 1-frame-latest
    /// snapshot, identical to the pre-P4 behavior in HW-less envs).
    fn decode_at_mf(
        &mut self,
        video_source_id: VideoSourceId,
        source_path: &Path,
        target_micros: u64,
        slot_idx: u8,
        forward_budget_micros: u64,
    ) -> Result<DecodedFrame, String> {
        // docs/plan_video_perf.md P1: lazy-init D3D11 + IMFDXGIDeviceManager
        // on the first decode. Once attempted, never retry — even if it
        // failed we just stay on SW decode for the rest of the process
        // (= no point spamming `D3D11CreateDevice` per frame).
        if !self.d3d11_init_attempted {
            self.d3d11_init_attempted = true;
            match try_init_d3d11() {
                Ok(state) => {
                    tracing::info!(
                        "video playback: D3D11 HW decode enabled (= MF_SOURCE_READER_D3D_MANAGER)"
                    );
                    self.d3d11 = Some(state);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "video playback: D3D11 init failed, falling back to SW decode"
                    );
                }
            }
        }
        // Split-borrow so the d3d11 reference is independent of the
        // self.readers borrow below (= `Entry` API takes &mut self).
        let Self { readers, d3d11, .. } = self;
        let d3d11_ref = d3d11.as_ref();

        // Lazy-init the reader for this source. `Entry::Vacant` keeps
        // a single hash lookup (clippy `map_entry`).
        let entry = match readers.entry(video_source_id) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let entry = create_reader_for_source(source_path, d3d11_ref)?;
                tracing::info!(
                    video_source_id,
                    width = entry.width,
                    height = entry.height,
                    hw_decode = d3d11_ref.is_some(),
                    "video reader created"
                );
                v.insert(entry)
            }
        };

        // Decide whether to seek. Forward-step within `forward_budget_micros`
        // = keep ReadSample-ing (cheap). Backward or larger jump = seek.
        // Caller picks the budget per ring slot (slot 0 = default ~100ms,
        // slots 1..N = larger so low-fps sources forward-walk instead of
        // re-seeking each slot).
        let should_seek = match entry.last_decoded_micros {
            None => true,
            Some(last) if target_micros < last => true,
            Some(last) if target_micros.saturating_sub(last) > forward_budget_micros => {
                true
            }
            Some(_) => false,
        };
        if should_seek {
            seek_reader(&entry.reader, target_micros)?;
            // After seek WMF re-syncs to the previous keyframe; the
            // forward scan below catches us up to the target.
        }

        // Walk forward until we have a sample whose timestamp is at
        // or past target_micros. Bound the walk to ~4 seconds of source
        // at 60fps (= 240 frames) so a malformed stream doesn't spin
        // forever — typical H.264 keyframe intervals are 1-4 sec so
        // post-seek catch-up should always finish within this budget.
        //
        // **Skip the BGRA→RGBA copy for non-target frames**: the
        // intermediate samples are read just to advance the decoder
        // (= H.264 P-frames have to be decoded sequentially to get to
        // the target). Dropping the `IMFSample` releases the GPU/CPU
        // surface back to WMF without us touching 8MB of pixel data
        // 240 times in a row. This is the 2-second-scrub-lag fix
        // (2026-05-25 user report).
        let t_decode_start = std::time::Instant::now();
        let mut frames_walked: u32 = 0;
        let mut last_sample: Option<(IMFSample, u64)> = None;
        let mut chosen: Option<(IMFSample, u64)> = None;
        for _ in 0..240 {
            let Some((sample, ts_100ns)) = read_sample_only(&entry.reader)? else {
                break; // EOS
            };
            frames_walked += 1;
            let ts_micros = (ts_100ns.max(0) as u64) / 10;
            if ts_micros >= target_micros {
                chosen = Some((sample, ts_micros));
                break;
            }
            last_sample = Some((sample, ts_micros));
        }
        let t_walk_done = std::time::Instant::now();
        let (final_sample, final_ts) = chosen.or(last_sample).ok_or_else(|| {
            format!("no frame decoded for source {video_source_id} at {target_micros}μs")
        })?;
        let frame = sample_to_frame(&final_sample, entry, d3d11_ref, slot_idx)?;
        let t_swap_done = std::time::Instant::now();
        entry.last_decoded_micros = Some(final_ts);

        // Diagnostic: log decode + swap times for the first few frames
        // *of real playback* (target_micros > 0) so we can see whether
        // WMF picked hardware or software decode (HW ≈ 5-10ms / frame,
        // SW ≈ 30-80ms / frame at 1080p H.264). Skipping the
        // target_micros == 0 case avoids burning the warm-up budget on
        // the idle-at-beat-0 repeated-request pattern (the previous
        // smoke run showed 30 consecutive zero-target decodes before
        // Play was even pressed). Skips logging after the budget is
        // exhausted to keep release builds quiet.
        if target_micros > 0 && entry.timing_log_remaining > 0 {
            entry.timing_log_remaining =
                entry.timing_log_remaining.saturating_sub(1);
            let walk_ms = t_walk_done.duration_since(t_decode_start).as_millis();
            let swap_ms = t_swap_done.duration_since(t_walk_done).as_millis();
            tracing::info!(
                video_source_id,
                target_micros,
                final_ts,
                frames_walked,
                walk_ms = walk_ms as u64,
                swap_ms = swap_ms as u64,
                "decode timing"
            );
        }

        // `sample_to_frame` already built the right variant (Shared
        // or Bgra) based on whether the HW path was available — just
        // return it.
        Ok(frame)
    }
}

impl Default for VideoPlaybackEngine {
    fn default() -> Self {
        Self::new()
    }
}


/// Per-event alpha at the given clip-local beat, derived from
/// `fade_in_beats` / `fade_out_beats` with the event's own
/// `fade_in_curve` / `fade_out_curve`. Range `0.0..=1.0`. Outside
/// both fade regions returns `1.0` (= fully opaque).
///
/// docs/plan_video.md §4 P7: linear / s-curve / exp formulae match
/// `common::audio_render::fade_envelope` (the audio sibling), so
/// crossfade visuals stay in step with the audio engine's gain
/// envelope when the user fades both halves of a clip together.
/// tempo automation (SongTempo lane) があれば `TempoMap` を build する (= 映像
/// source 時間を tempo 積分で求めて音とズレないようにする、 A4 r.md #8)。 無ければ
/// `None` で constant bpm の高速経路を使う。 build は O(song length) だが tempo
/// automation を持つ曲のみ (一般の constant 曲は無コスト)。
fn song_tempo_map_if_automated(song: &Song) -> Option<common::tempo_map::TempoMap> {
    let automated = song.song_lanes.iter().any(|l| {
        l.enabled && matches!(l.target, common::model::AutomationTarget::SongTempo)
    });
    automated.then(|| common::tempo_map::TempoMap::from_song(song))
}

fn event_alpha(event: &VideoEvent, clip_local_beat: f64) -> f32 {
    let event_local = clip_local_beat - event.event_start_in_clip_beats;
    if event_local < 0.0 {
        return 0.0;
    }
    let mut alpha = 1.0_f32;
    if event.fade_in_beats > 0.0 && event_local < event.fade_in_beats {
        let progress = (event_local / event.fade_in_beats) as f32;
        alpha *= fade_curve_value(progress, event.fade_in_curve);
    }
    let event_remaining = event.event_length_beats - event_local;
    if event.fade_out_beats > 0.0 && event_remaining > 0.0
        && event_remaining < event.fade_out_beats
    {
        let progress = (event_remaining / event.fade_out_beats) as f32;
        alpha *= fade_curve_value(progress, event.fade_out_curve);
    }
    alpha.clamp(0.0, 1.0)
}

/// Single fade-curve evaluator. `progress` is `0..=1`, output is
/// `0..=1`. Mirrors `common::audio_render::fade_envelope` math.
fn fade_curve_value(progress: f32, curve: FadeCurve) -> f32 {
    let x = progress.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => 0.5 - 0.5 * (std::f32::consts::PI * x).cos(),
    }
}

/// docs/plan_video.md P5 perf: scale the WMF output down to this
/// long-edge before the sample reaches the CPU. The preview window
/// caps at 960px in width by default ([`view::preview_window::
/// scale_to_fit_on_screen`]), so a 1920x1080 source decoded at native
/// would upload 8 MB / frame and waste ~4x more CPU + GPU bandwidth
/// than the eye ever consumes. 960 also keeps decode under ~5 ms /
/// frame on modern Intel iGPUs, which is the budget the GUI thread
/// has at 30fps preview without going chunky. Sources whose long
/// edge is already ≤ 960 are passed through native (= no upscale).
const PREVIEW_MAX_LONG_EDGE: u32 = 960;

fn scale_for_preview(native_w: u32, native_h: u32) -> (u32, u32) {
    let long = native_w.max(native_h);
    if long <= PREVIEW_MAX_LONG_EDGE {
        return (native_w.max(1), native_h.max(1));
    }
    let scale = PREVIEW_MAX_LONG_EDGE as f64 / long as f64;
    let w = ((native_w as f64) * scale).round().max(1.0) as u32;
    let h = ((native_h as f64) * scale).round().max(1.0) as u32;
    // Round to even so 4:2:0 / NV12 downstream consumers don't trip
    // on odd dimensions. Preview pipeline is RGB so this is just
    // hygienic.
    (w & !1, h & !1)
}

fn create_reader_for_source(
    path: &Path,
    d3d11: Option<&D3D11WmfState>,
) -> Result<ReaderEntry, String> {
    // MFStartup is owned by `import_video` and idempotent.
    crate::import_video::ensure_mf_startup_pub()
        .map_err(|e| format!("MFStartup: {e}"))?;

    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let url = PCWSTR::from_raw(wide.as_ptr());

    // docs/plan_video_perf.md P1: pre-allocate two attribute slots so
    // we have room for `MF_SOURCE_READER_D3D_MANAGER` when D3D11 is up.
    // Excess slots cost nothing if unused.
    //
    // **Video processing mode**: when D3D11 is attached we must use
    // `ENABLE_ADVANCED_VIDEO_PROCESSING` (= D3D-aware path), NOT the
    // basic `ENABLE_VIDEO_PROCESSING` flag. MSDN states these two are
    // mutually exclusive; combining basic + D3D manager returns
    // `E_INVALIDARG (0x80070057)` from `MFCreateSourceReaderFromURL`.
    let attrs = unsafe {
        let mut a = None;
        MFCreateAttributes(&mut a, 2)
            .map_err(|e| format!("MFCreateAttributes: {e}"))?;
        let attrs = a.ok_or_else(|| "MFCreateAttributes returned null".to_string())?;
        if let Some(d3d11) = d3d11 {
            // HW path: advanced video processing (D3D-aware) + D3D manager.
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(|e| {
                    format!("SetUINT32 ENABLE_ADVANCED_VIDEO_PROCESSING: {e}")
                })?;
            attrs
                .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &d3d11.manager)
                .map_err(|e| format!("SetUnknown MF_SOURCE_READER_D3D_MANAGER: {e}"))?;
        } else {
            // SW path: basic video processing (pre-P1 behaviour).
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .map_err(|e| format!("SetUINT32 ENABLE_VIDEO_PROCESSING: {e}"))?;
        }
        attrs
    };
    let reader: IMFSourceReader = unsafe {
        MFCreateSourceReaderFromURL(url, &attrs)
            .map_err(|e| format!("MFCreateSourceReaderFromURL: {e}"))?
    };

    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let native = unsafe { reader.GetNativeMediaType(stream, 0) }
        .map_err(|e| format!("GetNativeMediaType: {e}"))?;
    let frame_size = unsafe { native.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| format!("MF_MT_FRAME_SIZE: {e}"))?;
    let native_w = (frame_size >> 32) as u32;
    let native_h = (frame_size & 0xFFFF_FFFF) as u32;
    if native_w == 0 || native_h == 0 {
        return Err(format!("invalid frame size {native_w}x{native_h}"));
    }

    // Minimal output type — request only RGB32 subtype + the major
    // type. Empirically WMF's video processor MFT accepts this on
    // every H.264 / HEVC source we tested and falls back to native
    // dimensions automatically.
    //
    // **NB**: Earlier attempts to ask WMF to scale down at decode time
    // (by also setting `MF_MT_FRAME_SIZE` to a target like 960x540)
    // returned `MF_E_INVALIDMEDIATYPE` (0xC00D36B4) on the user's
    // 1920x1080 60fps source even with INTERLACE_MODE + FRAME_RATE +
    // PIXEL_ASPECT_RATIO populated — the video processor MFT seems to
    // accept format conversion but not arbitrary scaling for this
    // codec/driver combination. Preview throughput is therefore
    // limited by native-resolution decode; the proper fix is the
    // background worker thread described in `docs/plan_video.md` §3
    // (= lookahead ring buffer), which sits above this layer.
    let output = unsafe {
        let t = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("set MAJOR_TYPE: {e}"))?;
        // docs/plan_video_perf.md P3: ARGB32 (= BGRA, alpha-aware) so the
        // video processor MFT writes alpha=0xFF for opaque H.264 source.
        // Earlier RGB32 (= BGRX, alpha undefined) worked for the CPU path
        // because we hardcoded alpha=0xFF during the channel swap, but
        // broke the zero-copy `Shared` path: `CopySubresourceRegion`
        // copies bytes verbatim and the GPU view as `Bgra8UnormSrgb`
        // then read the X bytes as alpha=0, making every pixel fully
        // transparent (= dark backdrop only, preview blank).
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)
            .map_err(|e| format!("set SUBTYPE ARGB32: {e}"))?;
        t
    };
    unsafe { reader.SetCurrentMediaType(stream, None, &output) }
        .map_err(|e| format!("SetCurrentMediaType RGB32: {e}"))?;

    // Read back the delivered frame size — even when we don't request
    // scaling, WMF may pad e.g. 1080→1088 to satisfy H.264 macroblock
    // alignment. Trust the post-Set output type so the buffer math
    // below uses the actual delivered dimensions.
    let actual_size = unsafe {
        let cur = reader
            .GetCurrentMediaType(stream)
            .map_err(|e| format!("GetCurrentMediaType after Set: {e}"))?;
        cur.GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| format!("output MF_MT_FRAME_SIZE: {e}"))?
    };
    let actual_w = (actual_size >> 32) as u32;
    let actual_h = (actual_size & 0xFFFF_FFFF) as u32;

    // `scale_for_preview` is kept as a pure helper for future use
    // (e.g. once we have an explicit video processor MFT) — silence
    // the unused-fn warning by referencing it here.
    let _ = scale_for_preview;

    Ok(ReaderEntry {
        reader,
        width: actual_w,
        height: actual_h,
        last_decoded_micros: None,
        // Log the first ~60 playback decodes (= ~2 seconds at the
        // project's 30fps target). Skipping target_micros == 0 in
        // `decode_at` means this budget is reserved for real playback,
        // not idle-at-beat-0 spam.
        timing_log_remaining: 60,
        // Lazily created on first HW-decoded sample (= when D3D11
        // manager was attached and the reader returned an IMFDXGIBuffer).
        shared_pool: None,
    })
}

/// `IMFSourceReader::SetCurrentPosition` with the default 100-ns time
/// format (`GUID_NULL`). PROPVARIANT carries the position as VT_I8
/// (signed 8-byte int) per the WMF docs.
fn seek_reader(reader: &IMFSourceReader, target_micros: u64) -> Result<(), String> {
    let position_100ns: i64 = (target_micros as i64).saturating_mul(10);
    // PROPVARIANT_0 is a union holding `ManuallyDrop<PROPVARIANT_0_0>`,
    // so writing into the inner struct field-by-field trips the
    // "cannot DerefMut a ManuallyDrop union field" check. Build the
    // inner struct value and replace the whole union variant in one
    // assignment — equivalent to the C-level `PropVariantInit + set
    // tag + set value` idiom but typed.
    let propvar = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: position_100ns },
            }),
        },
    };
    let time_format = GUID::zeroed(); // GUID_NULL → 100-ns
    unsafe { reader.SetCurrentPosition(&time_format, &propvar) }
        .map_err(|e| format!("SetCurrentPosition: {e}"))?;
    Ok(())
}

/// Pull one decoded sample and return the `(IMFSample, timestamp)`
/// pair without copying its pixel content. Returns `Ok(None)` on EOS.
/// Skips STREAMTICK gaps internally. Used by `decode_at`'s forward
/// walk so intermediate P-frames don't pay the 8 MB BGRA→RGBA copy.
fn read_sample_only(reader: &IMFSourceReader) -> Result<Option<(IMFSample, i64)>, String> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    loop {
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else {
            // STREAMTICK or format change — drain again.
            continue;
        };
        return Ok(Some((sample, timestamp)));
    }
}

/// Turn one already-decoded `IMFSample` into a `DecodedFrame`. Two
/// underlying paths exist, selected at runtime by whether the buffer
/// is GPU-backed:
///
/// - **`Shared`** (zero-copy, P3): `IMFDXGIBuffer::GetResource()` →
///   ID3D11Texture2D → `CopySubresourceRegion` into the per-source
///   pool slot (`entry.shared_pool.slots[slot_idx]`). The pixel
///   bytes never touch CPU memory. Returned with the stable shared
///   HANDLE for that slot that the main thread imports into wgpu
///   exactly once per `(source_id, slot_idx)`.
/// - **`Bgra`** (P2 fallback): `ConvertToContiguousBuffer()` →
///   `IMFMediaBuffer::Lock()` → memcpy. Used when D3D11 init failed
///   or the H.264 MFT chose a system-memory output despite
///   `MF_SOURCE_READER_D3D_MANAGER` being attached. Bgra ignores
///   `slot_idx` (= the ring buffer is HW-path only; HW-less fallback
///   keeps the original 1-frame-latest semantics).
///
/// docs/plan_video_perf.md P4: `slot_idx` selects which slot of the
/// per-source `SharedPool` receives the decoded pixels. The worker
/// round-robins through `0..PREVIEW_RING_SIZE` when filling a ring
/// snapshot so consecutive frames land in independent textures.
fn sample_to_frame(
    sample: &IMFSample,
    entry: &mut ReaderEntry,
    d3d11: Option<&D3D11WmfState>,
    slot_idx: u8,
) -> Result<DecodedFrame, String> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;

    // GPU path: only when D3D11 is up AND the buffer is DXGI-backed.
    if let Some(d3d11) = d3d11
        && let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>()
    {
        let handle = write_to_shared_pool_slot(&dxgi, entry, d3d11, slot_idx)?;
        return Ok(DecodedFrame::Shared {
            width: entry.width,
            height: entry.height,
            handle: SendHandle(handle),
            slot_idx,
        });
    }

    // CPU fallback: existing P2 path.
    let bgra = sample_buffer_to_bgra(&buffer, entry.width, entry.height)?;
    Ok(DecodedFrame::Bgra {
        width: entry.width,
        height: entry.height,
        bgra,
    })
}

/// CPU fallback: lock the IMFMediaBuffer, copy the BGRA bytes into a
/// `Vec<u8>`, unlock. No channel swap (= P2: `upload_texture_bgra`
/// takes BGRA directly, the shader samples a `Bgra8UnormSrgb` texture).
fn sample_buffer_to_bgra(
    buffer: &windows::Win32::Media::MediaFoundation::IMFMediaBuffer,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let frame_bytes = width as usize * height as usize * 4;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)) }
        .map_err(|e| format!("Lock: {e}"))?;

    if ptr.is_null() || (cur_len as usize) < frame_bytes {
        let _ = unsafe { buffer.Unlock() };
        return Err(format!("frame too small: {cur_len} < {frame_bytes}"));
    }
    let src = unsafe { std::slice::from_raw_parts(ptr, frame_bytes) };
    let bgra = src.to_vec();
    let _ = unsafe { buffer.Unlock() };
    Ok(bgra)
}

/// docs/plan_video_perf.md P4 GPU zero-copy ring path: copy the
/// decoded HW sample's D3D11 texture into the per-source pool slot
/// at `slot_idx`, returning its stable NT handle.
///
/// On first call for a `ReaderEntry`, allocates the entire
/// `SharedPool` (= `PREVIEW_RING_SIZE` independent slots) via
/// `create_shared_pool`. Subsequent frames pick the slot indexed by
/// the caller (= worker's round-robin counter) and overwrite its
/// contents in place.
///
/// Keyed-mutex protocol: worker `AcquireSync(0, INFINITE)` →
/// `CopySubresourceRegion` → `ReleaseSync(0)` **on the targeted
/// slot only**. **The matching half is inside wgpu's DX12 / Vulkan
/// importer**, not in daw_01 code — proven empirically by removing
/// the worker pair (`c2ae697`) and observing the imported texture
/// render as fully transparent (reverted in `6b5eebd`, 2026-05-26).
/// The daw_01 main thread does NOT need to call `IDXGIKeyedMutex`
/// APIs.
fn write_to_shared_pool_slot(
    dxgi: &IMFDXGIBuffer,
    entry: &mut ReaderEntry,
    d3d11: &D3D11WmfState,
    slot_idx: u8,
) -> Result<HANDLE, String> {
    if (slot_idx as usize) >= PREVIEW_RING_SIZE {
        return Err(format!(
            "slot_idx {slot_idx} out of range (PREVIEW_RING_SIZE = {PREVIEW_RING_SIZE})"
        ));
    }
    // Extract the WMF-owned source texture + subresource index.
    let source: ID3D11Texture2D = unsafe {
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut ptr)
            .map_err(|e| format!("IMFDXGIBuffer::GetResource: {e}"))?;
        if ptr.is_null() {
            return Err("GetResource returned null".to_string());
        }
        ID3D11Texture2D::from_raw(ptr)
    };
    let subresource: u32 = unsafe { dxgi.GetSubresourceIndex() }
        .map_err(|e| format!("GetSubresourceIndex: {e}"))?;

    // Lazy-create the per-source pool on first HW frame for this
    // reader.
    if entry.shared_pool.is_none() {
        entry.shared_pool =
            Some(create_shared_pool(&source, d3d11, entry.width, entry.height)?);
    }
    let pool = entry
        .shared_pool
        .as_ref()
        .ok_or_else(|| "shared pool init failed".to_string())?;
    let slot = &pool.slots[slot_idx as usize];

    // Acquire the keyed mutex on **this slot only** before writing.
    // `INFINITE` waits for the main thread's render submit to
    // release; in steady-state this is sub-millisecond because the
    // main thread releases immediately after queueing the
    // textured-quad command, and slots not currently being presented
    // are simply free.
    unsafe { slot.mutex.AcquireSync(0, u32::MAX) }
        .map_err(|e| format!("worker mutex AcquireSync: {e}"))?;

    unsafe {
        d3d11.context.CopySubresourceRegion(
            &slot.texture,
            0,
            0,
            0,
            0,
            &source,
            subresource,
            None,
        );
    }
    // Flush so the GPU queue picks up the copy before the main
    // thread tries to sample. Without this the main thread might
    // observe an older frame for several render cycles.
    unsafe { d3d11.context.Flush() };

    unsafe { slot.mutex.ReleaseSync(0) }
        .map_err(|e| format!("worker mutex ReleaseSync: {e}"))?;

    Ok(slot.handle)
}

/// Allocate one `SharedSlot`: a `B8G8R8A8_UNORM` D3D11 texture with
/// `SHARED_NTHANDLE + SHARED_KEYEDMUTEX`, plus its `IDXGIKeyedMutex`
/// view and the NT handle from `IDXGIResource1::CreateSharedHandle`.
/// Called `PREVIEW_RING_SIZE` times per `ReaderEntry` (= once per
/// pool slot during `create_shared_pool`).
fn create_shared_slot(
    source: &ID3D11Texture2D,
    d3d11: &D3D11WmfState,
    width: u32,
    height: u32,
) -> Result<SharedSlot, String> {
    // Start from the source texture's desc so we inherit mip level
    // details, then override flags for sharing.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    desc.Width = width;
    desc.Height = height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    // docs/plan_video_perf.md P3: destination is `B8G8R8A8_UNORM` so
    // gui_01 can view it as `Bgra8UnormSrgb` (= same compatibility
    // class). WMF's `MFVideoFormat_ARGB32` source is also BGRA, so
    // `CopySubresourceRegion` copies the alpha bytes verbatim — and
    // the video processor MFT writes alpha=0xFF for opaque H.264
    // source, which is the whole point of choosing ARGB32 over RGB32.
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    desc.CPUAccessFlags = 0;
    desc.MiscFlags = (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
        | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
        as u32;

    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        d3d11
            .device
            .CreateTexture2D(&desc, None, Some(&mut texture))
    }
    .map_err(|e| format!("shared CreateTexture2D: {e}"))?;
    let texture = texture.ok_or_else(|| "shared texture null".to_string())?;

    // Cast → IDXGIResource1 for CreateSharedHandle, → IDXGIKeyedMutex
    // for AcquireSync / ReleaseSync. Both interfaces are inherited
    // by an ID3D11Texture2D created with the SHARED_* flags.
    let dxgi_resource: IDXGIResource1 = texture
        .cast()
        .map_err(|e| format!("IDXGIResource1 cast: {e}"))?;
    let mutex: IDXGIKeyedMutex = texture
        .cast()
        .map_err(|e| format!("IDXGIKeyedMutex cast: {e}"))?;

    // `DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE` = 1 | 2.
    // The constants are in `Win32::Graphics::Dxgi` but flagged as
    // `GENERIC_ALL` here for simplicity — the kernel just stores the
    // ACL mask, both processes are the same daw_gui.exe so we have
    // full access anyway.
    const DXGI_SHARED_RESOURCE_READ: u32 = 0x80000000;
    const DXGI_SHARED_RESOURCE_WRITE: u32 = 1;
    let handle = unsafe {
        dxgi_resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
            PCWSTR::null(),
        )
    }
    .map_err(|e| format!("CreateSharedHandle: {e}"))?;

    Ok(SharedSlot {
        texture,
        mutex,
        handle,
    })
}

/// Allocate a fresh `SharedPool` with `PREVIEW_RING_SIZE` independent
/// `SharedSlot`s. Each slot is sized to the source frame's actual
/// dimensions (= `ReaderEntry.width / height`); the worker's
/// `CopySubresourceRegion` writes into one of these on each decoded
/// sample, round-robin'd by `slot_idx`.
fn create_shared_pool(
    source: &ID3D11Texture2D,
    d3d11: &D3D11WmfState,
    width: u32,
    height: u32,
) -> Result<SharedPool, String> {
    // Build all N slots up front so the worker never has to allocate
    // mid-decode. `std::array::try_from_fn` is unstable on stable Rust,
    // so we collect into a Vec then convert — `PREVIEW_RING_SIZE` is a
    // const so the conversion is bounded and any size mismatch is a
    // compile-time invariant violation surfaced at the `try_into`.
    let mut slots: Vec<SharedSlot> = Vec::with_capacity(PREVIEW_RING_SIZE);
    for _ in 0..PREVIEW_RING_SIZE {
        slots.push(create_shared_slot(source, d3d11, width, height)?);
    }
    let slots: [SharedSlot; PREVIEW_RING_SIZE] = slots
        .try_into()
        .map_err(|_| "SharedPool slot count mismatch".to_string())?;
    Ok(SharedPool { slots })
}

/// BGRA8 → RGBA8 channel swap with alpha pinned to 0xFF. Picks the
/// fastest available path: SSSE3 `_mm_shuffle_epi8` on x86_64 (~10x
/// faster than scalar for 1080p), scalar otherwise. Pure function +
/// allocation-free input + caller-owned output Vec. Both paths are
/// covered by `bgra_to_rgba_*` unit tests for correctness.
pub fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    let len = src.len();
    debug_assert!(
        len.is_multiple_of(4),
        "BGRA input must be multiple of 4 bytes"
    );

    // `vec![0; len]` boils down to `memset` which is ~50 GB/s on a
    // modern CPU — adding 0.2 ms for an 8 MB 1080p frame, well under
    // 1 % of the channel-swap budget. Cheaper than the `set_len` +
    // write-everything-before-read pattern that clippy (rightfully)
    // flags as UB-prone.
    let mut dst = vec![0u8; len];

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("ssse3") {
            // SAFETY: feature detected at runtime, src + dst same len.
            unsafe { bgra_to_rgba_ssse3(src, &mut dst) };
            return dst;
        }
    }

    bgra_to_rgba_scalar(src, &mut dst);
    dst
}

/// Scalar fallback for `bgra_to_rgba`. ~15ms for 1080p on a typical
/// Skylake-class CPU. Used when SSSE3 isn't available (= ARM, very
/// old x86), or as the reference impl for the SIMD path's unit test.
fn bgra_to_rgba_scalar(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 0xFF;
    }
}

/// SSSE3-accelerated BGRA→RGBA. Processes 4 pixels (16 bytes) per
/// iteration via `_mm_shuffle_epi8`, then `_mm_or_si128` to set the
/// alpha lanes to 0xFF. ~1.5ms for 1080p, ~6ms for 4K — both well
/// under one 30fps frame budget.
///
/// # Safety
///
/// - CPU must support SSSE3 (caller verifies via
///   `is_x86_feature_detected!("ssse3")`).
/// - `src` and `dst` must have the same length and not overlap.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn bgra_to_rgba_ssse3(src: &[u8], dst: &mut [u8]) {
    use core::arch::x86_64::{
        __m128i, _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_setr_epi8,
        _mm_shuffle_epi8, _mm_storeu_si128,
    };
    debug_assert_eq!(src.len(), dst.len());

    // Shuffle mask: read from BGRA positions (2, 1, 0, _) per pixel.
    // `-1` clears the alpha byte; we OR in 0xFF below.
    let shuffle_mask = _mm_setr_epi8(
        2, 1, 0, -1,
        6, 5, 4, -1,
        10, 9, 8, -1,
        14, 13, 12, -1,
    );
    // alpha_or = 0x_FF_00_00_00 per 32-bit lane (little-endian = byte 3 = 0xFF).
    let alpha_or = _mm_set1_epi32(0xFF00_0000_u32 as i32);

    let chunks = src.len() / 16;
    let src_ptr = src.as_ptr() as *const __m128i;
    let dst_ptr = dst.as_mut_ptr() as *mut __m128i;
    for i in 0..chunks {
        // SAFETY: i < chunks ⇒ offset is in bounds for both pointers,
        // alignment is not required for `_mm_loadu_si128` (the u =
        // unaligned).
        unsafe {
            let v = _mm_loadu_si128(src_ptr.add(i));
            let shuffled = _mm_shuffle_epi8(v, shuffle_mask);
            let with_alpha = _mm_or_si128(shuffled, alpha_or);
            _mm_storeu_si128(dst_ptr.add(i), with_alpha);
        }
    }

    // Tail: 1080p / 4K / 720p are all multiples of 16, but be
    // defensive — scalar handle the last 0..15 bytes.
    let processed = chunks * 16;
    if processed < src.len() {
        bgra_to_rgba_scalar(&src[processed..], &mut dst[processed..]);
    }
}

/// Pull one decoded sample. Returns `Ok(None)` on EOS, `Ok(Some(...))`
/// for a real sample. Skips STREAMTICK gaps internally.
#[allow(dead_code)]
fn read_one_frame(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let frame_bytes = width as usize * height as usize * 4;
    loop {
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else {
            // STREAMTICK or format change — drain again.
            continue;
        };

        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)) }
            .map_err(|e| format!("Lock: {e}"))?;

        if ptr.is_null() || (cur_len as usize) < frame_bytes {
            let _ = unsafe { buffer.Unlock() };
            return Err(format!(
                "frame too small: {cur_len} < {frame_bytes}"
            ));
        }
        let src = unsafe { std::slice::from_raw_parts(ptr, frame_bytes) };
        let mut rgba = Vec::with_capacity(frame_bytes);
        // BGRA → RGBA channel swap. The alpha byte in WMF's
        // `MFVideoFormat_RGB32` is documented as undefined (per MSDN:
        // "Media Foundation might or might not preserve its value")
        // — we hardcode 0xFF here so the texture renders opaque. If
        // we used `px[3]` instead, the alpha would often be 0 and the
        // entire preview would be invisibly blended out (= the exact
        // "preview shows nothing" bug 2026-05-25).
        for px in src.chunks_exact(4) {
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(0xFF);
        }
        let _ = unsafe { buffer.Unlock() };
        return Ok(Some((timestamp.max(0) as u64, rgba)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, Clip, ClipContent, Song, VideoContent, VideoEvent, VideoSource,
        VideoSourcePath,
    };

    fn song_with_video_clip(bpm: f32, video_source_id: VideoSourceId) -> Song {
        let mut song = Song {
            bpm,
            ..Song::default()
        };
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: video_source_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    source_start_micros: 0,
                    source_end_micros: 4_000_000, // 4s
                    ..VideoEvent::default()
                }],
            }),
        );
        song.media.video_sources.insert(
            video_source_id,
            VideoSource {
                path: VideoSourcePath::Absolute("/dev/null".into()),
                width: 320,
                height: 240,
                framerate: 30.0,
                duration_micros: 4_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let mut track = crate::app::track_with(|t| {
            t.id = 1;
            t.name = "V".into();
        });
        track.clips.push(Clip {
            id: 1,
            start_beat: 4.0,
            length_beats: 8.0,
            content_id,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        });
        song.tracks.push(track);
        song
    }

    #[test]
    fn active_source_at_returns_none_outside_clip() {
        let song = song_with_video_clip(120.0, 1);
        // playhead before clip start
        assert!(VideoPlaybackEngine::active_source_at(&song, 0.0).is_none());
        // playhead after clip end
        assert!(VideoPlaybackEngine::active_source_at(&song, 100.0).is_none());
    }

    /// r.md #8 A4: tempo automation 下では映像 source 時間が tempo 積分で進む
    /// (= 一定 bpm 換算の `progress*60/bpm` とズレ、 映像が音と同期し続ける)。
    #[test]
    fn active_source_at_honors_tempo_automation() {
        // base 60bpm の video clip (clip/event は beat 4 始まり) に、 60→180 linear
        // の SongTempo lane [0,12) を載せる。 beat 4..8 は 100..140 bpm。
        let mut song = song_with_video_clip(60.0, 1);
        song.length_beats = 12.0;
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 60.0, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 12.0, value: 180.0, curve: AutomationCurve::Linear },
                ],
                next_point_id: 3,
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "t".into(),
                start_beat: 0.0,
                length_beats: 12.0,
                content_id: cid,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 60.0)
        });
        let (_id, micros) = VideoPlaybackEngine::active_source_at(&song, 8.0).unwrap();
        let secs = micros as f64 / 1_000_000.0;
        // 期待値 = tempo 積分した beat 4→8 の実時間。
        let m = common::tempo_map::TempoMap::from_song(&song);
        let expected = m.beat_to_seconds(8.0) - m.beat_to_seconds(4.0);
        assert!((secs - expected).abs() < 0.02, "secs={secs} expected={expected}");
        // 一定 60bpm 換算 (4 拍 = 4.0s) より明確に短い (テンポが速いので)。
        assert!(secs < 3.5, "tempo-integrated should beat constant-60 (4.0s), got {secs}");
    }

    #[test]
    fn active_source_at_returns_source_inside_clip() {
        // 120 bpm, clip starts at beat 4 (= 2s), playhead at beat 5 (=
        // 2.5s) → clip-local = 1 beat = 0.5s = 500_000μs.
        let song = song_with_video_clip(120.0, 7);
        let result = VideoPlaybackEngine::active_source_at(&song, 5.0)
            .expect("clip should be active at playhead 5.0");
        assert_eq!(result.0, 7);
        // Allow ±1μs rounding from f64 → u64.
        assert!(
            (result.1 as i64 - 500_000_i64).abs() <= 1,
            "expected ~500_000μs, got {}",
            result.1
        );
    }

    #[test]
    fn active_source_at_skips_audio_tracks() {
        let mut song = song_with_video_clip(120.0, 1);
        // Stick an Audio track at the top — must be skipped.
        let audio_track = crate::app::track_with(|t| {
            t.id = 2;
            t.name = "A".into();
        });
        song.tracks.insert(0, audio_track);
        let result = VideoPlaybackEngine::active_source_at(&song, 5.0);
        assert!(result.is_some(), "video track should still be found");
        assert_eq!(result.unwrap().0, 1);
    }

    #[test]
    fn active_source_at_honors_event_muted() {
        let mut song = song_with_video_clip(120.0, 3);
        // Mute the only event → no source returned.
        let cid = song.tracks[0].clips[0].content_id;
        let Some(ClipContent::Video(content)) = song.clip_contents.get_mut(&cid) else {
            panic!("expected Video content");
        };
        content.events[0].muted = true;
        assert!(VideoPlaybackEngine::active_source_at(&song, 5.0).is_none());
    }

    #[test]
    fn decode_at_returns_rgba_frame_at_target_micros() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("decode_at: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("playback.mp4");
        // 2-second 320x240 H.264 source with a smooth green gradient
        // so any decoded frame's center pixel reads as green-ish.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=green:size=320x240:duration=2:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success());

        let mut engine = VideoPlaybackEngine::new();
        // Decode the frame at 1 second (= middle of the clip).
        let frame = engine
            .decode_at(42, &mp4, 1_000_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 1s");
        assert_eq!(frame.width(), 320);
        assert_eq!(frame.height(), 240);
        // The HW path (DecodedFrame::Shared) keeps pixels on the GPU
        // so we can't inspect them from CPU. Only assert pixel colour
        // on the CPU-fallback path (= test environment without a GPU,
        // or D3D11 init failure). Either variant must report the
        // correct dimensions.
        if let DecodedFrame::Bgra { bgra, .. } = &frame {
            assert_eq!(bgra.len(), 320 * 240 * 4);
            // Center pixel should be green-ish. BGRA memory order:
            // px[0] = B, px[1] = G, px[2] = R, px[3] = (X / alpha
            // undefined per MSDN).
            let center = (120 * 320 + 160) * 4;
            let b = bgra[center];
            let g = bgra[center + 1];
            let r = bgra[center + 2];
            assert!(g > 100, "center G should be high, got ({r}, {g}, {b})");
            assert!(g > r && g > b, "green channel should dominate");
        }

        // Forward-step (= no seek): decode at 1.05s. Should reuse the
        // existing reader and just ReadSample forward — verified
        // implicitly by the result being a valid frame (no error).
        let frame2 = engine
            .decode_at(42, &mp4, 1_050_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 1.05s (forward step)");
        assert_eq!(frame2.width(), 320);
        assert_eq!(frame2.height(), 240);

        // Backward seek: decode at 0.3s. Engine should SetCurrentPosition
        // back to a keyframe and re-walk. Same validity check.
        let frame3 = engine
            .decode_at(42, &mp4, 300_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 0.3s (backward seek)");
        assert_eq!(frame3.width(), 320);
        assert_eq!(frame3.height(), 240);
    }

    // ====================================================================
    // bgra_to_rgba (2026-05-25 playback コマ送り fix): SSSE3 SIMD path
    // is selected at runtime when available, scalar otherwise. Both
    // paths must produce byte-identical output.
    // ====================================================================

    #[test]
    fn bgra_to_rgba_swaps_channels_and_pins_alpha() {
        // 1 pixel: BGRA (10, 20, 30, 99) → RGBA (30, 20, 10, 255).
        let src = [10, 20, 30, 99];
        let rgba = bgra_to_rgba(&src);
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn bgra_to_rgba_handles_pure_colors() {
        // Pure blue BGRA = (255, 0, 0, _) → RGBA = (0, 0, 255, 255).
        let src = [255, 0, 0, 0];
        assert_eq!(bgra_to_rgba(&src), vec![0, 0, 255, 255]);
        // Pure red BGRA = (0, 0, 255, _) → RGBA = (255, 0, 0, 255).
        let src = [0, 0, 255, 0];
        assert_eq!(bgra_to_rgba(&src), vec![255, 0, 0, 255]);
        // Pure green BGRA = (0, 255, 0, _) → RGBA = (0, 255, 0, 255).
        let src = [0, 255, 0, 0];
        assert_eq!(bgra_to_rgba(&src), vec![0, 255, 0, 255]);
    }

    #[test]
    fn bgra_to_rgba_4_pixel_block_matches_scalar() {
        // 16-byte block = exactly one SSSE3 iteration. Verify SIMD
        // and scalar produce identical output.
        let src: Vec<u8> = (0..16).collect();
        let rgba = bgra_to_rgba(&src);
        let expected = vec![
            2, 1, 0, 255, // pixel 0
            6, 5, 4, 255, // pixel 1
            10, 9, 8, 255, // pixel 2
            14, 13, 12, 255, // pixel 3
        ];
        assert_eq!(rgba, expected);
    }

    #[test]
    fn bgra_to_rgba_large_buffer_handles_tail() {
        // Non-multiple-of-16: 5 pixels = 20 bytes (= 1 SSSE3 chunk +
        // 4-byte scalar tail). All 5 pixels should be converted.
        let src: Vec<u8> = (0..20).collect();
        let rgba = bgra_to_rgba(&src);
        // Pixel 4 (tail) bytes 16..20 = (16, 17, 18, 19) BGRA →
        // (18, 17, 16, 255) RGBA.
        assert_eq!(rgba.len(), 20);
        assert_eq!(&rgba[16..20], &[18, 17, 16, 255]);
    }

    #[test]
    fn bgra_to_rgba_scalar_path_matches_simd_path_random() {
        // Cross-check: synthesize 1024 random-ish bytes, run both
        // paths, assert byte-equality. Catches any mis-translation
        // of the shuffle mask vs the scalar indexing.
        let src: Vec<u8> = (0..1024).map(|i| ((i * 37) ^ 0xA5) as u8).collect();
        let simd_out = bgra_to_rgba(&src);
        let mut scalar_out = vec![0u8; src.len()];
        bgra_to_rgba_scalar(&src, &mut scalar_out);
        assert_eq!(simd_out, scalar_out, "SIMD and scalar paths must agree");
    }

    #[test]
    fn scale_for_preview_caps_long_edge() {
        // 1920x1080 → scaled to 960x540 (= long-edge 960, aspect kept).
        assert_eq!(scale_for_preview(1920, 1080), (960, 540));
        // 4K → 960x540 too (1920/3840 = 0.5).
        assert_eq!(scale_for_preview(3840, 2160), (960, 540));
        // Already small → identity (with even-round hygiene).
        assert_eq!(scale_for_preview(640, 480), (640, 480));
        assert_eq!(scale_for_preview(960, 540), (960, 540));
        // Portrait → long-edge cap on height.
        assert_eq!(scale_for_preview(1080, 1920), (540, 960));
        // Odd dimensions get rounded to even (NV12 hygiene).
        let (w, h) = scale_for_preview(1921, 1081);
        assert!(w % 2 == 0 && h % 2 == 0, "even-aligned, got {w}x{h}");
    }

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

    // ====================================================================
    // P7: multi-clip composite (active_sources_at) + per-event alpha.
    // ====================================================================

    #[test]
    fn active_sources_at_returns_empty_when_no_video_active() {
        let song = song_with_video_clip(120.0, 1);
        // playhead outside clip → empty
        assert!(VideoPlaybackEngine::active_sources_at(&song, 0.0).is_empty());
        assert!(VideoPlaybackEngine::active_sources_at(&song, 100.0).is_empty());
    }

    #[test]
    fn active_sources_at_returns_single_with_alpha_one_outside_fades() {
        // No fade_in / fade_out → alpha == 1.0 everywhere inside the clip.
        let song = song_with_video_clip(120.0, 5);
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 1);
        let frame = active[0];
        assert_eq!(frame.video_source_id, 5);
        assert_eq!(frame.z_index, 0);
        assert!(
            (frame.alpha - 1.0).abs() < 1e-6,
            "outside-fade alpha = 1.0, got {}",
            frame.alpha
        );
    }

    #[test]
    fn active_sources_at_applies_linear_fade_in() {
        // 8-beat event with fade_in=2 beats. At clip-local 1 beat
        // (half through the fade), Linear curve → alpha == 0.5.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_in_beats = 2.0;
            c.events[0].fade_in_curve = common::model::FadeCurve::Linear;
        }
        // clip starts at beat 4, playhead at 5 → clip-local = 1 beat
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 1);
        assert!(
            (active[0].alpha - 0.5).abs() < 1e-3,
            "linear fade-in midpoint should be 0.5, got {}",
            active[0].alpha
        );
    }

    #[test]
    fn active_sources_at_applies_scurve_fade_out() {
        // 8-beat event with fade_out=2 beats. clip ends at beat 12,
        // playhead at 11 → 1 beat remaining → SCurve mid → 0.5.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_out_beats = 2.0;
            c.events[0].fade_out_curve = common::model::FadeCurve::SCurve;
        }
        let active = VideoPlaybackEngine::active_sources_at(&song, 11.0);
        assert_eq!(active.len(), 1);
        assert!(
            (active[0].alpha - 0.5).abs() < 1e-3,
            "scurve fade-out midpoint should be 0.5, got {}",
            active[0].alpha
        );
    }

    #[test]
    fn active_sources_at_composites_multi_track_bottom_up() {
        // Stack 2 video tracks with clips covering the same playhead.
        // Bottom track gets z_index=0, top gets z_index=1.
        let mut song = song_with_video_clip(120.0, 1);
        // Add a second video track at the top with its own source.
        let cid2 = song.alloc_content_id();
        song.clip_contents.insert(
            cid2,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: 2,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    source_start_micros: 0,
                    source_end_micros: 4_000_000,
                    ..VideoEvent::default()
                }],
            }),
        );
        song.media.video_sources.insert(
            2,
            VideoSource {
                path: VideoSourcePath::Absolute("/dev/null2".into()),
                width: 640,
                height: 480,
                framerate: 30.0,
                duration_micros: 4_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let top_track = crate::app::track_with(|t| {
            t.id = 2;
            t.name = "VTop".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 4.0,
                length_beats: 8.0,
                content_id: cid2,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
            t.next_clip_id = 2;
        });
        // Insert at position 0 = top of arrangement.
        song.tracks.insert(0, top_track);

        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 2, "both tracks should be active at 5.0");
        // z_index=0 is the bottom track (original source_id=1),
        // z_index=1 is the top (source_id=2). Caller renders in
        // ascending z_index so the source_id=2 layer ends up on top.
        assert_eq!(active[0].video_source_id, 1, "bottom track first");
        assert_eq!(active[0].z_index, 0);
        assert_eq!(active[1].video_source_id, 2, "top track second");
        assert_eq!(active[1].z_index, 1);
    }

    #[test]
    fn active_sources_at_drops_muted_events() {
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].muted = true;
        }
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert!(active.is_empty(), "muted event should be dropped");
    }

    #[test]
    fn active_sources_at_drops_zero_alpha_events() {
        // fade_in=2, position right at clip start → progress=0 →
        // alpha=0 → drop the frame entirely.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_in_beats = 2.0;
        }
        // clip starts at beat 4 — playhead exactly at 4 = clip-local 0
        let active = VideoPlaybackEngine::active_sources_at(&song, 4.0);
        assert!(active.is_empty(), "alpha=0 frame should be dropped");
    }

    #[test]
    fn active_source_at_clamps_to_event_end_micros() {
        // Playhead right at the end of the event — source_micros should
        // clamp to source_end_micros (= 4_000_000 here, not extrapolate
        // beyond).
        let song = song_with_video_clip(120.0, 1);
        // 8 beats long at 120bpm = 4s. Clip starts at beat 4. End is
        // beat 12 (just after, so use beat 11.9 to stay inside).
        let result = VideoPlaybackEngine::active_source_at(&song, 11.9)
            .expect("should be inside clip");
        assert!(
            result.1 <= 4_000_000,
            "source_micros should be clamped to source_end_micros, got {}",
            result.1
        );
    }
}
